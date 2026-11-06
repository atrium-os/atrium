# Atrium Navigator — backend/frontend split, and the headless contract

**Status:** design, not built. Companion to [atrium-navigator.md](atrium-navigator.md)
(the D6 thesis: a browser is a JS runtime by accident; the jail replaces the
in-process sandbox). That document says *what* the browser dissolves into. This one
says *where the seam goes*, so the backend can be built, tested and fuzzed with no
display server, no compositor and no UI — and the UI attached afterwards as a second
consumer of an interface the tests already exercise.

---

## 1. The invariant

> **The UI process holds no authority and contains no parser.**
> Everything that touches untrusted bytes, and everything that holds a capability,
> lives behind the seam.

This is deliberately *not* the conventional "engine in the back, chrome in the front"
split, which draws the line at rendering rather than at trust. It is stated as an
invariant because it is testable: delete the UI, replace it with a shell script, or run
none at all, and neither the security posture nor the test suite changes. §8's M5 gate
is exactly that assertion.

It also follows from the D6 thesis. If the jail is the sandbox, the thing that must be
jailed is whatever parses. The UI parses nothing; it is a viewport and an intent source.

---

## 2. Trust topology — the backend is not one process

A single headless daemon would reproduce the monolith it replaces. The backend is a
broker plus short-lived jailed workers, one per unit of untrusted input.

| component | responsibility | capabilities |
|---|---|---|
| **`navigatord`** (broker) | sessions, history, policy decisions, store handle, worker lifecycle | store + jail-spawn. **No network. No parser.** |
| **fetcher** (jailed, per request) | bytes off the wire; TLS; DNS | **network only, zero filesystem** |
| **document worker** (jailed, per document) | HTML/CSS/MD → scene graph | **zero capabilities** — bytes in on a pipe, scene graph out |
| **UI** | pixels, input, user intent | none |

Notes on the shape:

- **One jail per document is the default, not a mitigation.** Mainstream browsers
  needed Site Isolation as a retrofit because documents share an address space. Here
  documents never do, so cross-document memory disclosure is structurally absent rather
  than mitigated. The cost that made it prohibitive elsewhere — process launch — is a
  Portcullis jail here, and is a budget to measure (§8 M3), not an assumption.
- **The fetcher holds the only network capability.** A document worker cannot reach the
  network at all, so "document exfiltrates what it parsed" has no vehicle: its single
  output is a scene graph on a pipe.
- **`navigatord` holds authority but never parses.** The two dangerous jobs — holding
  capabilities, and consuming hostile input — are never in the same process. The one
  place that consumes worker output is the validator of §5, which is small, total and
  fuzzed precisely because it is the exception.

---

## 3. Two planes

**Control plane** — `navigatord` ↔ UI over Aqueduct. Requests (`open_session`,
`navigate`, `back`, `forward`, `close`) and an event stream back (`resolved`,
`fetching`, `scene_ready`, `blocked`, `needs_consent`, `failed`). A script drives it;
no UI required. Because it is Aqueduct, a remote UI is the same code path — the
location-transparency property atrium-navigator.md §3.1 already claims, obtained rather
than added.

**Content plane** — the document worker publishes its scene graph as a Fresco surface,
composed into the chrome by **Limen**. Document pixels never round-trip through the UI
process, and chrome and document are separate Fresco subtrees with separate trust: the
`<iframe>` replacement, used for the top-level document too.

**Headless mode** substitutes "serialize the scene graph to NSG on stdout" for "attach
to Fresco". That single substitution is what makes the whole backend testable without a
display server, and it is the only difference between the test configuration and the
shipping one.

---

## 4. The scene graph is the boundary

Fresco is a retained **scene-graph** server and NSG (the Navigator Scene Graph, §4.1a) is a serialized scene graph, so the
document pipeline is a pure function:

```
bytes + viewport + font set  →  scene graph
```

No pixels, no GPU, no compositor. Style and layout are tested by comparing data
structures, not by screenshotting — the property a conventional browser engine cannot
have, and the reason "build the backend properly first" is achievable here rather than
aspirational.

### 4.0 What the converter hands over: the recording

The pure function above takes *bytes*. For the legacy lane it takes something slightly
richer, and leaving that undocumented would make the converter's output a private format
between two components that are built years apart. `navigator-prerender` emits it today as
`atrium-navigator-recording/2` (§4.5 covers the version bump and what version 1 still guarantees).

A recording is **one document plus a table of what a reader can do to it**:

```json
{ "format": "atrium-navigator-recording/2",
  "url": "https://example.test/page",
  "tier": 1,
  "tier_reason": "conversion removed reader-visible content",
  "measurements": { "elements": 1183, "text_before": 14654, "text_after": 5063,
                    "scripts_total": 70, "scripts_failed": 0,
                    "interactive_found": 42, "transitions_dropped": 3 },
  "transitions": [
    { "trigger": "#menu-toggle", "event": "click", "anchored": true,
      "attribute_only": true,
      "effects": [ { "kind": "attribute", "target": "#menu", "name": "class",
                     "from": "nav hidden", "to": "nav" } ] } ],
  "document": "<html>…" }
```

Four properties of it are load-bearing, and each was learned rather than designed:

**The tier and its reason travel with the bytes.** The converter measures and a policy
decides (legacy-web spec §5.4.2); publishing the chosen document without saying which
pipeline produced it would make a demoted artifact indistinguishable from a successful
conversion of a page that happens to be short.

**93% of transitions carry no content.** Their effect is a single attribute write, because
what they reveal is already in the document — the commonest interactive element on the web
is a class toggle. The backend does not need a second DOM to make a menu work; it needs to
apply one attribute. This is what makes the §5 interaction vocabulary cheap enough to be
worth having.

**`anchored` says whether a transition can be replayed at all.** A trigger the page's own
scripts created does not exist in a tier 1 document, and a positional path may address a
different node there. Unanchored transitions are reported rather than dropped, and a
demotion keeps only the anchored, id-addressed ones — with the discarded count in
`transitions_dropped`, because a recording that silently lost half its entries looks
identical to a page with little to do.

**Field order is fixed and the bytes are reproducible.** The same document converts to the
same recording, which is what lets Tessera key it by content hash (§5). That required
making the clock, `Math.random` and `crypto` deterministic — real entropy in any one of
them defeats dedup as thoroughly as all three.

The validator (§4.2) applies to the `document` field exactly as it would to any other scene
graph input; the transition table is subject to the same treatment, since a recording
arriving from a shared store is no more trusted than the page it came from.

**One parser, and serialize→reparse is a fixed point.** The converter serializes its DOM
into `document`; the backend re-parses those bytes and walks the tree positionally to
resolve a recorded trigger. Two parsers would eventually disagree about the same bytes, so
there is exactly one — the `navigator-dom` crate, shared by both sides. That is necessary
but not sufficient: the serializer must also be the parser's inverse, or the tree the
backend walks is not the tree the converter measured.

It was not, and the failure was silent in exactly the way that matters — triggers that
simply did not resolve, on 12 of 19 for one document, with nothing logged. Three causes,
all found by re-parsing the corpus's own recordings and comparing round 1 to round 2:

- `<script>` and `<style>` text was HTML-escaped on output, so `(()=>{` returned as
  `(()=&gt;{` and then `(()=&amp;gt;{`. Embedded JavaScript was corrupted and every
  document grew about 250 KB per round.
- `<noscript>` is raw text too when scripting is enabled, which it is here; same defect,
  smaller. `<textarea>` and `<title>` are deliberately *not* in that set: they are
  escapable raw text, where entities are decoded on parse and must be re-escaped on output.
- Attribute names ran the other way. HTML parsing lowercases them; the converter's DOM kept
  whatever case a script assigned, so a `tabIndex="-1"` it wrote came back as
  `tabindex="-1"`. The DOM now lowercases attribute names on HTML elements, as
  `setAttribute` does — and exempts foreign content, because SVG's `viewBox` is not
  `viewbox` and lowercasing it breaks the graphic silently.

With those closed, all 103 recordings in the corpus are fixed points and every recorded
trigger resolves. The property is worth stating as a requirement rather than a bug fix:
**a recording's `document` must parse to a tree that serializes back to the same bytes.**
A converter that cannot meet it is emitting a document its own reader cannot navigate.

### 4.1 Hermetic rendering (design in from M0, painful to retrofit)

Golden-file tests are worthless if the output is not byte-stable. Hermetic mode pins:
a fixed font set (shipped, versioned, never the system's), a fixed viewport and device
pixel ratio, no clock, no network, no randomness, and deterministic iteration order in
every map or set that reaches layout.

**Byte-stability is M0's gate, before any real content support** — three runs on two
machines, identical bytes. A pipeline that is nondeterministic at M0 stays that way, and
every later golden test inherits the flakiness.

### 4.1a NSG, and M0: Markdown → NSG

**NSG — the Navigator Scene Graph.** It is the serialized scene graph that headless mode
writes and golden tests compare. (Earlier drafts called it "OTL", which collides with
Orbis's tile format, magic `OTL1`. Renamed with the user, 2026-09-22.)

**What a node carries is decided by who may parse fonts.** Web fonts are parsed and
shaped only in the worker, and Fresco never receives a web font file (Profile v1, web
fonts, condition 1). Layout must also measure text in order to break lines. So the worker
**shapes**, and NSG carries **shaped glyph runs**: the font by content address, size,
colour, and glyph ids with positions. Rasterizing glyphs is a separate, later stage.
Fresco's `OP_TEXT_RUN_INSTALL`, which shapes a string server-side by font *name*, suits
shipped UI fonts. It cannot carry a document.

**Hermetic by construction, not by care:**
- **Integer geometry.** Every position and size is an integer in **1/64 px**. Shaping
  returns integer font units, and scaling is integer arithmetic with one stated rounding
  rule. There is no float formatting in the output, so there is none to differ between
  machines.
- **A pinned font set.** The shipped fonts go through the same canonicalization as web
  fonts (`navigator-fonts`: static, unhinted, sanitized), and the renderer **refuses to
  start** if a font's canonical address differs from the pinned constant. A font change
  is therefore a visible version bump, never a silent drift.
- **A fixed viewport**, no clock, no network, no randomness, and output in document
  order.

**The serialization is text**, one node per line, so a golden diff reads as a layout
change. `nsg 0.2` heads the file, followed by `viewport`, `font`, `clip`, `group`,
`rect`, `run`, `link` and (since 2026-11-06) `path` lines. Each run carries its source text too, so a reader of a diff can see what
moved.

**The `path` node: vector shapes (2026-11-06, `navigator-render/src/path.rs`).** It is
one filled path, one to one with Fresco's `ATRIUM_CORE_FILL` (0x1004, fresco-protocol
`FillParams`; the coverage rule is fresco-compute's `fill`):

```
path <evenodd 0|1> <a> <b> <c> <d> <e> <f> <rrggbbaa> <opacity 0-255> v<verbs> p<x>,<y> … [c<i>] [g<i>] [x<i>]
```

The transform's linear part is in 1/65536 and its offset in 1/64 px. The verbs are
fresco-compute's (0 move, 1 line, 2 quad, 3 cubic, 4 close), the points are in 1/64 of a
path unit, and the colour is sRGB with straight alpha.

*Where paths come from.* The normalizer keeps a standalone inline SVG's text on its
replaced element (`atrium-svg`). The renderer turns it into paths (navigator-svg
`vector`, after the same strict profile as the bitmap path):
- a stroke, with its dashes, becomes the filled outline it paints (tiny-skia's stroker);
- a transform becomes the fill's own affine;
- group opacity over one shape folds into its paint.

*Paint, clips, and drops (2026-11-06, after Fresco's gradients landed, 3892556).*
- *Gradients.* A paint is solid, or linear or radial: Fresco's `FillPaint`, at most 16
  stops, pad spread. In NSG it is `L<x0>,<y0>,<x1>,<y1>` or `R<cx>,<cy>,<r>`, in 1/64
  path unit, then `;<rrggbbaa>@<offset/1024>` per stop.
- *Colour spaces differ by paint.* A solid colour goes to Fresco in linear RGB. Gradient
  stops stay sRGB, because Fresco interpolates them premultiplied in sRGB. Both are
  tested.
- *A linear gradient is exact under any invertible gradient transform.* Its parameter
  is still an affine function of position, so its end points are solved for
  (p₀ = T q₀, direction L⁻ᵀd/|d|²); they are not merely mapped. A non-uniform
  `objectBoundingBox` and a skew are tested against the SVG's own parameter.
- *A radial gradient goes vector only under a similarity*, with its focus at its centre.
- *Rectangle clips.* A clip path that is one axis-aligned rectangle, under axis-aligned
  transforms, becomes the fill's own clip rectangle (`k<x0>,<y0>,<x1>,<y1>` in 1/64 px).
  Nested clips intersect, and the scene's clip is intersected too. Fresco tests the
  pixel centre against it, with no antialiasing on the clip edge.
- *Drops.* Every vector SVG keeps its bitmap fallback in the scene (`path_fallbacks`;
  NSG `fallback p<first>+<count> <image>`). When Fresco reports fills dropped
  (`EV_WINDOW_FILLS_DROPPED`), the UI swaps that SVG's paths for the image, and swaps
  back on an empty report (`path::fallback_for`).

*What keeps the bitmap path*, with the reason counted:
- any other clip path, a mask, a filter;
- group opacity over several shapes;
- a pattern;
- a gradient with reflect or repeat spread, more than 16 stops, or (radial) a
  non-similar transform or a focus;
- an embedded image, or text.

*The checks.*
- The worker checks each path against Fresco's own limits at the painted scale
  (`fill::flatten` and `fill::pack`: at most 64K lines a path, at most 4,096 in one
  16 px tile).
- A document's SVGs may use at most half of a frame's 512K lines.
- A path that would pass any of these sends the whole SVG back to the bitmap path.
- `fill_params` makes a node what frescod takes.
- `validate` runs fresco-protocol's `FillParams::validate`, the same function frescod
  runs. The broker calls it before handing a node on.
- Scroll and zoom move a fill with `ATRIUM_CORE_FILL_TRANSFORM` (0x1005), without
  re-sending its geometry.

*In the review rasterizer* (`raster`), paths are painted by fresco-compute's own
coverage functions, with gradients evaluated as Fresco does (`path::paint_at`).

*Measured* (warm, all 122 pages):

| | inline SVGs drawn |
|---|---|
| **as vector shapes** (2,651 path nodes) | **1,962 (92.0%)**; with solid paint only it was 1,502 (70.4%) |
| kept as a bitmap: external sprite | 67 |
| kept as a bitmap: clip path (not a rectangle); 525 before rectangle clips | 66 |
| kept as a bitmap: filter | 12 |
| kept as a bitmap: embedded image | 8 |
| kept as a bitmap: mask | 7 |
| kept as a bitmap: group opacity over several shapes | 5 |
| kept as a bitmap: animation | 2 |
| kept as a bitmap: radial gradient under a non-uniform transform | 2 |
| kept as a bitmap: text | 2 |

- *Pages without inline SVG:* their NSG is byte-identical to 0beab757's. All 57 scenes
  that changed are pages with inline SVG.
- *Through frescod's own pipeline* (`navigator-render/tests/fresco_integration.rs`).
  - *The steps.* A page of inline SVGs (strokes, a dash, both gradients, a rectangle
    clip, opacity) is rendered into path nodes. Each becomes the `FillParams` the broker
    would send and passes `validate`. It is translated as frescod does
    (`translate_fill`), packed by `Frame::pack_report` as frescod packs a window, and
    rastered by frescod's real kernels.
  - *Tier-2* (the SPIR-V compiled by Cranelift, frescod's CPU renderer) is **identical
    to fresco-compute's reference, 0 pixels differing**, and the GIR interpreter agrees.
  - *A path too dense for one tile* is dropped by `pack_report` with reason 2 (tile too
    dense), and only it; the scene's bitmap fallback for its SVG is what the UI draws
    instead.
- *The worker's tile pre-check is best-effort.* Tile density depends on where a path
  falls on the 16 px grid, and scrolling moves that. The worker packs each path at the
  origin of the grid; frescod's drop report and the fallback are what make a dense path
  safe wherever it lands.
- *Not covered yet:* the socket, the scene server's envelope, the compositor and the
  `EV_WINDOW_FILLS_DROPPED` delivery, which need frescod running in the VM. frescod has
  no pixel readback, so that run would compare a QEMU screendump.

**The M0 gate:** the repo's own Markdown (the corpus) produces **byte-identical NSG over
3 runs on 2 machines**: macOS (host) and FreeBSD (the VM, Laminar, cross-built).

**M0's Markdown** is CommonMark via `pulldown-cmark` (MIT): headings, paragraphs,
strong, emphasis, code spans and blocks, lists, block quotes, links, thematic breaks
and tables. Images are out of scope, since they need the fetcher and intrinsic sizes.
Line breaking is greedy at break opportunities; total-fit (the profile's paragraph
algorithm) comes later, and the NSG does not change when it lands, only the positions do.

**M0 — PASSED (2026-09-22), `navigator-render`.** `nsg-render --corpus` over the repo's
111 tracked Markdown files gives digest `4a99561…e121c` on:
- **3 runs on the host** (macOS, aarch64), plus a fourth under a different `TZ` and `LANG`;
- **3 runs in the VM** (FreeBSD 16-CURRENT, Laminar, Tessera root, cross-built,
  hash-verified at the destination).

The VM also re-derived all six pinned font addresses; the renderer refuses to start
otherwise. **Controls:**
- changing one character in one file changes exactly that file's hash and the digest;
- the golden test fails on a doctored golden;
- a narrower viewport produces more lines, so layout is not a constant.

The first control was itself broken the first time: macOS `sed` does not support the
GNU-only `0,/re/` address, so the "changed" file was identical. The control reported
that, which is why it exists.

**Honest limits of the gate.** Both machines are aarch64. An x86-64 run was not
possible, since Rosetta is not installed on the host and installing it is a system
change. Integer-only geometry is what makes cross-ISA agreement *expected*; it is not
*demonstrated*.

What the corpus exercised that M0 does not render faithfully, counted by the renderer:
- 1,724 emphasis runs drawn upright (no italic face in the set);
- 432 lines over the viewport (code blocks do not wrap);
- 68 raw-HTML fragments skipped;
- 125 glyphs no face has, **all emoji** (✅ ×113, 🟡 🚫 ⏸ ⬜ 🔒 🚧). Colour emoji is outside
  Profile v1.

Speed: about 20 ms per document on the host. DejaVu, the fallback for ★, lives in
`test-assets/` without its licence file; it must move to `fonts/` with its licence
before this set ships.

### 4.2 The scene graph is untrusted input

The worker is jailed *because we assume it can be compromised*, so what it returns is
attacker-controlled. Before a graph reaches Fresco, `navigatord` validates it against
hard limits — node count, tree depth, image dimensions, total bytes, string lengths —
and rejects rather than clamps. This validator is the one place in the broker that
consumes hostile input: it must be small, total (no recursion without a depth bound), and
fuzzed with reached-coverage reported (§7).

**The document inside a recording gets the same treatment, and the split is: the converter
reports, the backend refuses.** Both read the Document Profile's ceilings from one place
(`navigator-dom`'s `profile` module), because a renderer that refuses documents its own
converter happily emits is not a safety property, it is a broken pipeline. What differs is
the decision. The converter still holds the document and can say *which* ceiling a real
page broke — that is how three of the ceilings were corrected — so it reports and
continues. The backend is promising G3, boundedness, about a document from a shared store;
a document outside the profile is exactly the one for which no bound was ever established,
so it refuses.

Order matters. Bytes are bounded during ingest, before parsing; element and depth ceilings
need a tree, and building a tree from unbounded bytes to discover whether the bytes were
bounded is the check defeating itself.

Every refusal test is paired with an acceptance test, and the acceptance arm runs at corpus
scale: all 99 recordings from the main corpus and all 67 from the adversarial one are
inside the profile. A validator that refused everything would pass every refusal test ever
written, and this project has already shipped a profile checker that checked nothing while
two whole corpora reported clean.

### 4.3 Replaying a transition

Applying a transition's effects is the first operation in the backend that *changes*
anything a reader will see, so it is the one that has to refuse.

**Application is atomic.** A transition is one observed step of a state machine: the
converter triggered something, diffed the tree, and recorded the whole difference.
Applying part of it produces a document nobody ever observed and nothing ever validated —
a menu marked open whose contents were never inserted. That is worse than refusing,
because it looks like a rendered page. Every effect is therefore checked before any effect
lands, the work happens on a copy, and the copy replaces the document only if all of it
succeeded. The copy *is* the mechanism: an undo log would put the correctness of a refusal
in the code path that runs only when something has already gone wrong.

**Four refusals, each naming itself:**

- *Unresolved trigger* — the transition was not recorded against this document at all.
  Reported as itself rather than as whatever its effects happen to fail on first.
- *Unresolved target* — an effect names a node this document does not have.
- *Failed precondition* — every attribute effect records what it replaces. A document that
  does not hold that value is not the document this was recorded against; the refusal
  carries both values so a caller can tell a stale recording from a tampered one. This is
  the check that makes a recording from a shared store safe to replay at all.
- *Incomplete* — the recording says effects were dropped when it was made, so it does not
  describe a complete step and cannot be replayed into one.

And the profile is re-checked on the **result**. Per-effect limits do not subsume it: 64
effects of 16 KiB of markup each is megabytes of growth, and the ceiling is a property of
the document, not of any single edit.

**Position is not recorded.** An `insert` names its parent, not its index among siblings,
so replay appends. Where ordering matters this is a fidelity loss, and it is stated here
rather than discovered later — the recording format would have to carry an index to fix it.

Measured: all 264 anchored transitions in the main corpus and both in the adversarial one
apply cleanly to the documents they were recorded against — every precondition holds,
every path resolves, and no result leaves the profile.

**Near-native replay, part 1 (2026-11-06; approved by the user).** The copy above made
every step cost a whole-document clone and a whole-tree profile walk. The same result is
now reached without either. The clone path stays as the reference; `History` uses the new
one.
- *Applied in place, refusals first.* `apply_in_place_with_undo` runs every check that
  can refuse before the first write:
  - trigger and target resolution;
  - attribute preconditions, on an overlay, so two effects on one attribute compose as
    they did on the copy;
  - that each removed node and each insert's parent is still connected, given the step's
    own earlier removals;
  - fragment parsing;
  - the profile.

  Only then are the writes made, in the same order and by the same calls. Nothing after
  the first write can fail. The undo is built from the same facts at the same moments
  (removed markup, parent and index taken before the writes; paths after them). So the
  all-or-nothing rule above holds without a copy, and it is not an undo log: no rollback
  code exists to be wrong.
- *The profile, incrementally* (`document::Stats`).
  - The element count changes by exactly what is removed and inserted.
  - Bytes, depth and stylesheets are kept as UPPER bounds, grown by bounds computed from
    the step: escaped attribute values; inserted markup at 8 bytes per byte plus 64;
    parent depth plus fragment depth; the fragment's sheets.
  - A step proven inside every ceiling skips the whole-tree walk.
  - A step that is not is applied by the copy path itself. So a refusal carries the exact
    measurement it always did, and the bounds are refreshed exactly.
- *Addresses compiled per document state* (`Document::ids`).
  - `#id` resolution scanned the whole arena for every address. A document state now
    resolves them through an index of the connected nodes that hold an id, built once
    and giving exactly `resolve_path`'s answer (the one such node, or none if several).
  - Positional paths still walk the tree. Path resolution remains the validation: an
    address that does not resolve refuses the step as before.
  - The index is dropped by any write that can change what a path names (a structural
    change, or an `id` written), and is rebuilt on next use. Only the apply paths write.
- *And one allocation removed* (`Dom::attr`): a name that needs no lowercasing is
  compared without building a lowercased copy. The `#id` scan made that copy for every
  node. This is shared with the converter, and its outputs are unchanged.

*Kept, byte for byte* (`tests/apply_in_place.rs`):
- every anchored transition of the 2026-11-05 recordings (31 documents), each applied
  and rewound from the base, then all as one long chain and rewound: **2,885 steps and
  2,885 rewinds**, the same document and the same undo as the clone path;
- 4,000 mutated transitions refusing identically, reaching every refusal (trigger,
  target, precondition, incomplete); and the outside-the-profile refusal through the
  exact path;
- a planted byte difference is caught.

*Measured* (`examples/nav_cost`, in-process, the same 2,242 steps; M4 Max):

| | go p50 / p99 | back p50 / p99 |
|---|---|---|
| clone (0beab757) | 3.47 / ~6–10 ms | 3.05 / ~5–12 ms |
| in place | 1.19 / 2.06 ms | 0.80 / 1.5 ms |
| + `attr` without allocation | 0.28 / 0.46 ms | 0.19 / 0.33 ms |
| **+ compiled addresses** | **0.6 / 22 µs** | **0.5 / 22 µs** |

A document's first step also compiles its state (the stats and the id index), once: p50
0.2 ms, p99 2 ms.

**Part 1b: the whole click (2026-11-06; asked by the user).** `nav_cost` timed the
transition alone, so the table above did not show what a reader's click cost. A new
example, `examples/click_cost`, times all of `Session::navigate` and `back` over the same
steps: 830 µs at p50. 99% of it was outside the transition.
- *The session's memory charge* (`remeasure`).
  - It serialized the whole document after every step, to learn its length. Even
    counting without building the string (`serialized_len`) was 565 µs.
  - The document now KEEPS its exact serialized length (`Document::bytes`). It is
    measured once, then changed by each in-place step by what that step wrote: the
    element's attribute list before and after, each removed subtree before it goes,
    each inserted subtree after it lands.
  - Each of those is measured by the serializer's own code
    (`Dom::attr_serialized_len`, `Dom::subtree_serialized_len`, one `RAW_TEXT` list),
    not a second implementation.
- *Finding the transition.* A scan of the table with a string compare per entry is now a
  hash map of offerable triggers (`Session::by_trigger`), first entry first as before.
  The click no longer clones the transition it applies.
- *Positional addresses* (`html:0>body:0>…`) are compiled per document state like `#id`
  ones (`Document::paths`). They are dropped by the same writes.
- *Inserted markup* is parsed once per distinct fragment (`Document::frags`), not at
  every click. Parsing is pure, so nothing invalidates it.

*Kept, byte for byte:*
- `tests/apply_in_place.rs` over the recordings (`NAVIGATOR_RECORDINGS`): every step and
  rewind is still the clone path's. The kept length now equals the real serialized
  length after each of them.
- A control that drops the insert accounting fails it.
- The 421-document identity set's normalized documents and NSG are unchanged.

*Measured* (3 interleaved runs per arm, the same 2,211 clicks and 2,242 backs; M4 Max):

| | click p50 / p90 / p99 | back p50 / p99 |
|---|---|---|
| before (f23419dd) | 829–832 / 1,464–1,469 / 1,485–1,496 µs | 823–829 / 1,483–1,500 µs |
| **after** | **0.79 / 1.54–1.58 / 20.8–21.0 µs** | **0.58 / 19.8–20.1 µs** |

A click is now what its transition costs. `History::go` alone is p50 0.6–0.7 µs, and its
p99 moved from 23 µs to 18 µs with the address and fragment caches. After a DOM step the
display update was then the whole remaining cost; part 2, below, deals with it.

**Part 2: after a step, only what changed is drawn again (2026-11-06; asked by the user).**
Target: a click's whole update (the DOM step, restyle, relayout, and the new scene of the
bands that changed) under 16 ms, byte for byte what a full relayout gives.

*Where a click's update went* (`examples/update_cost`, 33 corpus documents, 1,390 clicks and
backs, before this part): **p50 328 ms, p99 840 ms**. Normalizing was about 90% of it
(cascade 57 ms, margin collapsing 56, float fitting 84, emit 20, tables 17, box-sizing 15,
all p50). Then prepare (parse and the flat cascade) at 14 ms, and layout and NSG for the
view at 1 ms.

*Why the restyle is not incremental.* The page's cascade is the normalizer's, and every pass
after it works on the whole document: margin collapsing, float rows and float fitting,
table premeasure, box-sizing and emit. Emit names its flat classes in document order, so
one new style renames every later class, and a one-class toggle rewrites about the whole
normalized text. Restyling only dirty subtrees means rebuilding the normalizer as an
element-scoped compiler, which is a separate project (proposed below). Until then, a
step's new state is normalized whole, either at the click or ahead of it.

**How a change is shown** (`display::Bands::update`; the worker and the in-process host call
it whenever the session's generation moves):

1. *Proven inert: no work.* The document keeps a journal: each step bumps a version and
   records the attributes it wrote, whether it changed the tree's shape, and whether it
   wrote inside an `<svg>` (`Document::touched_since`). Neither the normalizer nor the
   renderer reads `tabindex`, `aria-*` or `data-*` by name; only a selector can. A step that
   wrote only those, outside any `<svg>`, to a document none of whose CSS mentions them
   (every supplied sheet and `<style>`, lowercased, escapes dropped), changes nothing drawn.
   Its update is nothing. (An inline SVG is serialized whole and addressed by hash, so no
   attribute inside one is inert.)
2. *A state known to look the same: no work.* A step the layout could not see joins the set
   of state keys known to lay out as the one shown. Going to any of them is nothing.
3. *A state laid out ahead: swapped in.* `History::key` chains the steps taken; a back
   restores the key it had. The worker keeps laid-out states by key:
   - the state just left;
   - in idle time, the states a click can reach. Each offerable transition is applied to
     a copy, normalized, prepared, and laid out for the current view
     (`Bands::precompute`, one per idle ask);
   - a state left by a click shown in place is turned into a whole source in idle time
     (`prepare_back`).

   A click to such a state swaps its source in (`DocumentSource::swap_in`) and sends only
   the bands whose bytes differ from what the view holds. The source it replaces is kept
   whole, so going back is a swap too.
4. *Otherwise: normalized now, laid out again only from where it changed*
   (`DocumentSource::replace`, `Banded::replace`):
   - `Prepared::difference` walks both states in document order. It compares each node's
     handle, kind, parent, attributes other than `class`, computed style and
     block-in-inline promotion. The text of a `<style>` element is not compared; its meaning
     is in the computed styles.
   - The layout restarts at the last checkpoint whose block precedes the first difference.
     Bands before it stay; the first pass resumes from there, lazily, as far as the view
     asks.
   - What later bands taught the kept ones is forgotten where the change can reach it:
     - every per-block record of a dropped band;
     - the final size of each box still open at the restart point. A kept band that
       painted one is stale and paints it again without one, as in a first pass that has
       not reached it.
     - the overlay leaves of kept bands inside an open box that positions or transforms
       what it holds, which are laid out again at once.
   - An open table keeps its row records (`row_cuts`, row lists) when no changed node
     differs in anything they read: `display`, `position`, overflow, percentage heights and
     offsets, `rowspan` (`Prepared::table_records_stand`). Rebuilding them walks the whole
     table.
   - The checkpoint's own block keeps where it began. Without it, a run into a 2,300-row
     table found its place again from the table's start: 450 ms instead of 17.

   Then the window is synced to the last `want`, and `band_updated` names exactly the bands
   whose range or NSG differ. The overlay is sent again only if its NSG differs.

*Found on the way, and fixed* (each was there before this part; the oracle found them):
- **A band laid out again was never sent again.** `sync` re-ran a stale resident band
  inside `ensure`, which cleared `stale`. Its "send what changed" check looked only at the
  band's foreign leaves, so the new scene, now painting a box sized by a later band, never
  reached the view. Neither did a re-run by `Banded::settle`. A band slot now counts its
  builds (`BandSlot::built`), and the view's record compares that count.
- **The same band had different bytes depending on its history.** A band's scene height,
  and the canvas rectangle sized from it, came from the document's estimated extent when
  the band was laid out. A first-pass run and a re-run also differed. A band's scene is now
  as tall as the band (the last band, as tall as what its run laid out). The overlay is as
  tall as what it holds, not the extent when it was last sent. Visible pixels are
  unchanged (the band pixel checks pass).

*Kept byte for byte* (`tests/replace.rs`, `NAVIGATOR_RECORDINGS`):
- *The relayout.* After every step and rewind, the view built from the updated source's
  messages (puts and clears applied to a mirror) holds exactly what a source built fresh on
  the new state gives it: every resident band's range and NSG, the overlay and the extent.
  This is checked at once, against a fresh source whose first pass reached the same band,
  and again after both settle, with every checkpoint and band range too. Three views: the
  page's top, a third of the way down, and where the reader sees each `#id` trigger.
- *The worker's path.* Inert steps, known-same states, swaps, prepared hits and misses,
  through the answers the worker sends, against a `Bands` opened fresh on the new state.
- A **broken control** (`bands::set_replace_control`: nothing laid out again) is caught.
- **0 differ.** The relayout was checked 4,314 times: the 1,390 steps and rewinds of the
  2026-11-05 explore recordings (858 and 532) and 48 on four big documents with
  synthetic clicks (`examples/synth_clicks`: hide and restyle at 10, 50 and 90% of the
  document), each at three views. The worker path covered 1,438 steps: 555 swaps or
  prepared hits, 148 proven inert, 546 known-same and 189 misses.
- After rebasing onto the dark-appearance change (`with_dark`), a subset was run again:
  batch 2 at two views, batch 1 and the big documents at the trigger, and the worker path
  over batch 2 and the big documents (580 steps). 0 differ.
- The broken control: 10 of 60 steps differ.

*Measured* (`examples/click_update`, in-process; the whole click: the step,
`Bands::update`, and the `want` answer with its NSG. The view is where the reader sees the
trigger for an `#id` trigger, else the page's first screen. M4 Max, nothing else running.)

| | p50 | p90 | p99 | max |
|---|---|---|---|---|
| **before** (part 1b: a click's display update, `update_cost`, 1,390 steps) | 328 ms | 801 ms | 840 ms | 1,036 ms |
| corpus, click laid out ahead (695 clicks, 2 runs) | **0.18 ms** | 3.1 ms | **9.0–9.3 ms** | 13.0–13.6 ms |
| corpus, back (1,390, 2 runs) | **0.01 ms** | 0.5 ms | **5.8 ms** | 13.0–13.4 ms |
| corpus, click not laid out ahead (695) | 5.7 ms | 764–791 ms | 785–816 ms | 990–1,038 ms |
| …of which normalized at the click (353) | 173–176 ms | 769–796 ms | 943–985 ms | 990–1,038 ms |
| big documents, back (48, view at the click) | 2.9 ms | 8.9 ms | 9.6 ms | 9.7 ms |
| big documents, click laid out ahead (War and Peace, commons: 12) | 1.3 ms | 3.1 ms | 14.6–14.9 ms | 14.9 ms |
| big documents, click normalized at it (24) | 1.48 s | 3.7 s | 3.7 s | 3.7 s |

Of the 695 corpus clicks not laid out ahead, 148 were proven inert and 187 reached a state
known to look the same, so they did no work (p50 5.7 ms is that half). The other 353 waited
for the normalizer. With the view where the reader sees the trigger the numbers are the
same as at the page's top. In run 1, one click of a document whose normalize sits at the
1 s gate was not built ahead (1,007 ms).

*The relayout alone* (the cold path's layout, against a full relayout to the same view;
`examples/relayout_cost`, 2 runs each):

| | incremental p50 / p90 / p99 / max | full relayout |
|---|---|---|
| corpus, 1,390 steps | 4.8 / 7.2–8.1 / 20 / 101–104 ms | 7.3–7.8 / 45 / 98–111 / 199–201 ms |
| big documents, 48 steps, view at the click | 6.9 / 28–29 / 34 / 34 ms | 21 / 362 / 635–641 / 635–641 ms |

*Memory.* A state laid out for a screen costs up to 24 times its normalized text
(`examples/state_memory`). The HN thread is 106 MB per state for 4.5 MB of text; War and
Peace is 7 MB for 3.9 MB. The worker keeps at most 6 states and an estimated 256 MiB:
six of a corpus page's, two of the HN thread's. It builds a state ahead only when it fits
beside the one just left. It also builds ahead only for a document that normalizes in under
1 s (`PRECOMPUTE_MS_MAX`): the worker answers one request at a time, and a click arriving
during a multi-second build would wait for it. The big documents' clicks therefore stay
cold. Moving the normalize to a helper thread would lift that limit.

*What is left.* A cold click is the normalizer, 175 ms at p50 on the corpus and 1.5–4 s on
the big documents. The fix is an incremental normalizer:
- the cascade element by element, with a rule index by subject (`sel::RuleIndex` is most
  of it);
- each later pass restricted to the boxes a change can reach (margin collapsing reaches
  siblings and the parent chain; float rows reach a parent; float fitting reaches a
  formatting context; tables reach a table);
- generated classes named by content, so a restyle edits the normalized text locally.

That is a project of its own, with this part's oracle as its test: a click's normalized
text must stay byte-identical to a whole normalize.

### 4.4 Going back

A reader moving back through states needs the state they were actually in, not one
reconstructed from a recording that may not describe it.

**The undo is derived from the document, not from the recording.** Everything an inverse
needs is present at the moment the effect is applied: a removal is about to destroy a
subtree that is right there, and an insertion chooses the position it lands at.
`applied_with_undo` reads it from the one source that cannot be wrong, and **every**
transition is exactly reversible — no fallback, no unreversible cases.

This replaced an earlier design that read the inverse out of the transition. That one could
invert an `attribute` effect (both sides are recorded) but not a `remove` (only a path is),
so a history kept a whole-document snapshot for those steps — measured at 7 of the corpus's
264 anchored transitions.

The obvious fix was to record more: put the removed markup in the recording. **That is the
wrong fix, and the reason generalises.** It would add a second copy of a derivable fact to
untrusted input, where it can disagree with the document — and a consumer holding two
versions of what used to be at a path must choose one with nothing to choose on. Derivable
facts do not belong in an untrusted format. The evidence is direct: **version 1 recordings,
which carry neither removed markup nor insert positions, reverse 264/264.**

An undo goes back through the ordinary path — preconditions, profile check, atomicity — with
one exception: the trigger is not required to resolve. That check asks whether a transition
was recorded against this document, which is already answered for an undo built from it, and
a step that removes its own trigger (a tab control replaced by the panel it opens) must
still be undoable.

Since 2026-11-06 the history goes back IN PLACE (`undo_in_place`, §4.3 part 1): the same
checks before any write, the same result byte for byte as `undone` (2,885 rewinds checked),
at p50 0.5 µs instead of 3 ms.

Reversal is verified by **byte equality** on the serialized document, at corpus scale:
259/259 on the main corpus, 2/2 on the adversarial one, and 264/264 on the older version 1
recordings. A weaker assertion — "the attribute is false again" — passes on a document that
has also quietly gained or lost something else, which is the failure an undo path has.

**The history is bounded, and says when the bound bit.** An undo holds the markup a removal
destroyed, so a session's history grows with the content it discards. Measured across the
corpus, an undo costs a median of **14 bytes**, p99 **2,193**, max **2,284** — the
overwhelming majority of transitions are attribute toggles, and an attribute undo is two
short strings. Every undo of every transition in all 98 documents totals 40 KB.

So the defaults are 64 steps and 1 MiB (an eighth of the document ceiling). The step count
is what binds on real content — 64 steps of measured traffic is about 140 KB — and the byte
bound engages only when a page removes large subtrees, which the corpus does not do but a
page is free to.

Two rules matter more than the numbers:

- **Forward motion is never blocked by the undo budget.** A step whose undo is too large to
  keep still happens; what it costs is the ability to come back from it. Refusing the step
  would let a page's own content decide whether a reader may turn the page.
- **`AtStart` and `Forgotten` are different answers.** A reader who has taken 80 steps under
  a 64-step bound and pressed back 64 times has not reached the beginning of the document.
  A boolean return would have told them they had — a lie by omission, and one the reader has
  no way to detect. The bound is allowed to forget; it is not allowed to pretend it did not.

### 4.4a How many sessions may exist

A session is one recording being read: the published document, the document as the reader
has changed it, and the history that gets them back. Nothing bounded how many could exist,
so "open recordings until the process dies" was a supported operation — and the memory is
not the caller's own, it is documents, which come from untrusted input.

**Derived from the corpus:** a document is a median of 97 KiB, p99 1.5 MiB, max 2.0 MiB,
against the profile's 8 MiB ceiling. A session's floor is about twice its document. The
defaults are **16 sessions and 64 MiB**. The count alone would be a promise the process
cannot keep — 16 sessions at the 8 MiB ceiling is 256 MiB — which is why there is a byte
budget too; at measured sizes those same 16 sessions cost about 3 MiB. Verified against the
corpus's **sixteen heaviest** documents opened at once, the adversarial ordering: they fit
in 51% of the budget.

**Refusal, not eviction.** Silently discarding a session resets a reader's place with no
signal they can act on — they return to a tab and it has forgotten where they were.
Refusing to open a new one is visible and recoverable: the caller can close something. An
automatic policy would need a measurement of real reader behaviour that does not exist yet,
and guessing one here would bury the guess where it is hardest to find.

**Mechanism here, policy with the caller.** `Sessions` refuses to exceed a bound and evicts
exactly what it is told to evict; it never chooses *which* session a reader should lose,
because it cannot know which window is in front of them. That is `navigatord`'s question —
the same split the converter and the tier policy already use.

Two things the errors have to get right:

- **Each refusal names its bound.** A caller told "too many sessions" closes one; a caller
  told "out of memory" may close several and still fail because the next document is simply
  too large. One merged error would make the right response unguessable.
- **The profile refusal comes first.** A caller told it is out of memory will close sessions
  to make room for a document that was never going to open.

A session's cost is measured as it moves, not fixed at its base — a reader who expands every
collapsed section holds more than was published. It is recomputed once per navigation rather
than per query, because measuring means serializing the tree, and doing it lazily made
opening one session cost a serialization of every other session's document.

### 4.4b How long a session lives

Bounding how many sessions exist does not bound how long one lives. A reader who closes a
laptop mid-article leaves a session holding a document indefinitely, and nothing reclaimed it.

**Two bounds, because one is not enough.** An idle timeout (default 30 minutes) catches the
reader who walked away. A maximum age (default 8 hours) catches what idleness cannot:
anything touching a session on a timer — a poll, a keep-alive, a page that moves itself — is
never idle, and *never idle* would mean *never reclaimed*.

**These two numbers are not derived, and should not be mistaken for the others here.** The
byte and count bounds come from corpus measurement; there is no corpus of reader behaviour,
so 30 minutes and 8 hours are conventional. What would change them is telemetry from real
sessions, which does not exist and is not being invented to justify a number.

**The library never reads a clock.** Every entry point that can expire a session takes `now`
from the caller. Tests are then exact and instant rather than sleeping; the converter already
had to make its clock injectable for byte-reproducible output, and a second component
reaching for wall time would undo that lesson locally; and `navigatord` owns the lifecycle,
so it owns the clock. A backwards step — NTP moving a wall clock — reads as no time passing,
which keeps a session open rather than vanishing it under a reader.

Expiry is caller-driven like eviction: there is no background thread, because a library that
spawned one would be choosing a runtime for its embedder. But **`open` sweeps before it
refuses** — turning a reader away because of a session they abandoned an hour ago would be
the bound working against the person it protects.

**An expired session is not an unknown one.** A returning reader is told "that timed out"
and offered it back, not "no such thing" — the same distinction as `Forgotten` versus
`AtStart` in §4.4. The memory of expiries is itself bounded (default 64), so it cannot become
the leak it was added to explain; past that boundary the answer honestly becomes `Unknown`.

### 4.6 The broker, and the seam that makes §1 testable

`navigatord` drives sessions through the control-plane vocabulary of §3: `OpenSession`,
`Navigate`, `Back`, `Close`, `Report`, `Activate`, `Image` in; `SessionOpened`,
`SceneReady`, `Rewound`, `Closed`, `Expired`, `Blocked`, `NoSuchSession`, `Report`,
`Link`, `Image` out.

`Activate { session, href }` is a reader following a link in the scene. The broker
decides where it goes (`links.rs`, threat model §2.10): it resolves the href against the
document's URL with the WHATWG parser, the one parse it does, on a string rather than a
document. Then it answers with one of three `LinkAction`s: `InDocument`, `Navigate`
(with `insecure` set for `http`), or `Handoff`, which is only for a scheme the user
allowlisted. Anything else is `Blocked`. A jailed host supplies the base URL over the
worker pipe (`URL`). A worker that lies about it can do no more than a page writing
absolute links already could. And the href must be one the document carries: spelled in
the recording the broker holds, and on an `<a>`/`<area>` of the current document
(threat model §2.10).

**Progressive display (2026-11-05, legacy-web §5.4.5.7).** Three more requests, so a
reader never waits for the whole conversion: `Upgrade { session, recording }` hands the
broker the scripted conversion's readable recording for a session showing the tier-1
one; `Swap { session, anchor }` comes from the UI at an idle moment, carrying where the
reader is; `Attach { session, transitions }` hands over the transition table, which
arrives after exploration. Answers: `UpgradeReady` / `UpgradeKept` (not meaningfully
different: nothing on screen changes), `Replaced { anchor }` (the same reading position
in the new document), `TransitionsAttached`, `TransitionsHeld`. The comparison, the
anchor mapping and the digest binding live in `progressive.rs`; both hosts implement
them, the jailed one over the worker wire (`UPGR`, `SWAP`, `ATCH`), with the broker
keeping its own copy of what it staged so what a session carries follows what it shows.

**Images after the first display (2026-11-05).** The first document is shown before its
images are fetched: each `<img>` is `pending` in its recording, sized from the page's
own `width` and `height` (`declared`), from the image's own first bytes when the page
does not size it (`header`, 2026-11-06, legacy-web §5.4.5.7 cut A: exactly the size the
whole image will have, so it arrives with `resized` empty), or claiming no natural size
(`placeholder`: its header was not in hand in time; the box is what the page's CSS makes
it, else 300 × 150). An image whose header proves it will fail is left out, as the final
recording leaves it. `Images { session, images }`
hands the broker an images artifact as they arrive: bound to the first document by
digest, carrying the whole images list so far, partial at most every 250 ms, then
complete. The answer is `ImagesAttached { arrived, resized, complete }`. `arrived` are
the addresses the session's recording now names, so their pixels may be asked for.
`resized` are the `src`s whose box may change: their natural size differs from the one
the shown layout used, or they failed and are gone. With `resized` empty the pixels
fill boxes already laid out, and nothing moves. Otherwise the UI lays the document out
again at an idle moment at the reader's anchor, which on an unchanged document is the
same element at the same offset, and presents it in one frame, as for a swap. What the
session carries, and what the store keeps, becomes the merged recording. Both hosts
implement it on the broker's side only (`progressive::FirstImages`); the worker's
document does not change. Refused: another document's images, any after the complete
one or after a swap, and a "complete" one with an image still pending.

`Image { session, address }` asks for one image's pixels. The broker checks that the
recording names the address and reads the blob from its store. The document's worker
decodes it, and the broker validates the RGBA8 buffer before it becomes
`Event::Image` (threat model §2.4).

**The broker names no document type.** It cannot call the parser, hold a DOM, or read a
recording's bytes, because it never sees any of those types — everything that does lives
behind a `DocumentHost` trait. §2's rule that the broker holds authority but never parses
stops being a discipline someone must remember and becomes something the type system
enforces. The test suite drives it only through requests and events, which is §1's claim
("delete the UI and neither the security posture nor the test suite changes") written down
as tests; a suite that poked at internals would pass equally against a broker with no seam.
A second host implementation that owns no parser at all is exercised in the tests, which is
the cheap half of the proof.

**What is not true yet:** `InProcessHost` runs in this process. There is no jail, no pipe,
no separate address space. The seam is real; the *isolation* is not, and will not be until a
host spawns a Portcullis jail and talks to it over a pipe. What exists today buys that the
swap changes one implementation and no broker logic.

Three behaviours are load-bearing:

- **Refusals are events, not returned errors.** A broker that returned `Result` to its UI
  would let a caller check the happy path and drop the rest; as events, a refusal travels
  the same channel as a success and carries a reason meant for a person.
- **A request is a moment in time.** `handle_at` takes the arrival time, so an expiry can
  fall due *because* a request arrived, and is delivered *alongside* that request's answer
  rather than instead of it. Expiries also reach the UI unprompted on a tick — a UI that
  learned of one only by failing a navigation would show a reader a page that is already
  gone and then take it away under them.
- **Only anchored triggers are offered.** An unanchored transition addresses a node the
  page's own scripts made, which is not in the published document; offering it would produce
  a refusal the reader could do nothing about.

End to end on real converter output: **98 sessions opened, 259 navigations, 259 rewinds, 0
blocked** — driving every trigger the broker itself offered, rather than a list the test
invented.

### 4.7 The document worker, and what "jailed" does not yet mean

`JailedHost` implements `DocumentHost` by running **one worker process per document**, with
a pipe as its only channel. The broker holds capabilities and never parses; the worker
parses and holds nothing — it opens no files, makes no network calls, holds no store
handle, and refuses a second document outright, because a worker that multiplexed sessions
would put two documents in one address space and reintroduce exactly the sharing Site
Isolation had to be retrofitted elsewhere to undo.

The broker did not change to gain this. The same requests over the same events produce, on
the corpus, the same numbers as the in-process host: **98 sessions, 259 navigations, 259
rewinds**. That is what the §4.6 seam was built to be able to say.

**What is NOT true yet, and must not be read as if it were.** `Confinement::None` is
*process isolation* — separate address space, separate crash domain, one document per
process. Those are real, and they are not a jail: no capability restriction, no filesystem
or network removal. "It is jailed" is not a uniform claim; a jail is a capability *set*, and
a host that said "jailed" while running a bare subprocess would be the most dangerous
comment in the tree. So the only constructor that produces it is named
`unconfined_for_testing`, a host reports `is_confined()` honestly, and
`require_confinement()` exists for a deployment that must refuse to start rather than run
open.

**Why the jail is not wired, rather than wired badly.** Portcullis today launches
*applications*: `portcullis launch <app-tree>` reads a signed `atrium.toml`, builds a
jail.conf section, and starts a long-lived jail. It has no "run this executable confined and
hand me its stdin/stdout" mode — which is precisely what a process-per-document host needs.
**That mode is a Portcullis-side change and is the next real step.** Inventing an invocation
here would produce a host that claims confinement and silently provides none, which is worse
than having neither. `Confinement::Launcher` takes the command explicitly and this crate
asserts nothing about what any given launcher confines.

**The pipe from a worker is untrusted input to the one process holding capabilities.** Spec
§2 jails the worker *because we assume it can be compromised*, so everything it writes back
is attacker-controlled — the length prefix included. Every read is bounded before a byte is
allocated (frame payloads and the header line alike, since a peer that never sends a newline
is a denial of service costing one byte a second). A reply the host does not recognise is a
failure, never a value to fall back on. And the memory budget is charged from bytes the
*broker* measured before spawning: a worker asked how large it is could answer zero, and the
limit protecting the broker would be set by the thing it protects against.

Three failures a worker can inflict, each survivable and each named: it **dies** (reported
as `Status::Failed`, distinct from an expiry, which is the system reclaiming an abandoned
session on purpose), it **hangs** (killed at a per-request deadline — a pipe has no read
timeout, so a reader thread makes "bound every request" a mechanism rather than a comment),
or it **lies** (retired, not believed).

### 4.7a Measured: the corpus through real FreeBSD jails

`Confinement::Launcher` pointed at `portcullis exec --instance {instance}` (portcullis.md
§6.5.2), on FreeBSD 16.0-CURRENT aarch64, everything cross-built on the host and staged by
scp. The worker is an installed, **signed** app whose manifest declares no capabilities at
all — no network, no mounts — with its library closure resolved into the tree by `opifex`.

| host | sessions | navigations | rewinds | failures | wall |
|---|---|---|---|---|---|
| in-process | 98 | 259 | 259 | 0 | 78 s |
| worker processes, unconfined | 98 | 259 | 259 | 0 | 56 s |
| **worker processes, jailed** | **98** | **259** | **259** | **0** | **59 s** |

98 jails created and destroyed, and afterwards **no jails, no mounts and no roots left
behind**. The broker is unchanged across all three rows: that is what the `DocumentHost`
seam was for.

**Three bugs this found, none visible without running it.**

1. **A launcher's chatter corrupts the protocol.** `jail(8)` prints `<name>: created` on
   *stdout* — the same pipe the worker speaks frames on. The broker read it as a length
   prefix and reported `malformed frame: bad length in "…: created"`. Anything a launcher
   emits on stdout is indistinguishable from payload; `jail -q` is load-bearing, and
   everything `portcullis exec` has to say goes to stderr.
2. **SIGKILLing a launcher leaks its jail.** `JailedHost` killed a retiring worker outright,
   so `portcullis exec`'s teardown never ran, and because a jail is created with
   `persist = true` the jail object and its mounts outlived it — after which the next
   session with that instance tag was refused, because the husk still answered to the name.
   Retirement now closes stdin first (the worker already exits on end-of-input), giving the
   launcher a bounded window to tear its jail down, with the kill as the backstop for a
   worker that ignores EOF. And `portcullis exec` treats a **process-less** jail as wreckage
   to reclaim rather than an instance to refuse, so one killed worker cannot poison its tag
   until a human notices.
3. **Piped stderr that nobody reads is worse than no stderr.** The host piped the worker's
   stderr and never drained it, so every word a failing worker said was discarded — and a
   worker chatty enough to fill the pipe buffer would have blocked forever on a write with
   no reader, which from the broker's side is a hang with no explanation. It is inherited
   now.

**What this does and does not prove.** It proves the mechanism end to end: signed manifest,
one jail per document, pipe protocol across the boundary, bounded requests, clean teardown
at scale. The *confinement* itself was verified separately with a shell app inside the same
jail configuration (`/etc`, `/usr`, `/var`, `/home` absent; writes land in tmpfs and vanish;
the app tree untouched) — not by probing from the Navigator's own worker, which speaks only
the frame protocol and cannot be asked what it sees.

### 4.7d The converter in a jail (threat model §2.2, built 2026-11-05)

The process that runs page JavaScript is now a one-shot jail entry, the same way the
document worker is (§4.7a). It is `org.atrium.navigator.converter`: a signed manifest
with **no capabilities**, its entry being `prerender` with no arguments. The batch driver
runs `PRERENDER_LAUNCHER="portcullis exec --daemon --instance conv{instance}
org.atrium.navigator.converter"` once per document.

**The protocol** (`navigator-prerender/src/serve.rs`). The child holds no filesystem and
no network, and reads no arguments or environment. So:
- stdin carries a strict header, `atrium-prerender/1` followed by tab-separated
  `key=value` fields; unknown keys are refused. The document bytes follow it.
- stdout carries `F <url>` fetch requests, the report lines `--one` always printed,
  `BLOB <blake3:hex> <n>` and `RECORDING <n>` frames, `TIMEOUT` and `END`.

**Why the fetcher is relayed rather than inherited.** `--daemon` hands the jail exactly
three descriptors over `SCM_RIGHTS` (portcullis.md §6.5.2a), so a fourth one, to fetchd,
does not survive the trip. Instead the unconfined driver starts the conversion's own
jailed fetchd (`PRERENDER_FETCHD`, one per conversion, so its §2.1 pins are that
conversion's alone). It forwards each `F` line to fetchd and each reply frame back,
verbatim, through the same bounded reader `FetchdFetcher` always used. The driver holds
nothing it did not hold before; the converter now holds nothing at all.

**What the driver does not believe.**
- Every stated length is bounded before allocation: blob ≤ 8 MiB, recording ≤ 128 MiB,
  report ≤ 8 MiB, line ≤ 16 KiB.
- A blob whose bytes do not hash to its claimed address fails the conversion. Storing it
  would let a compromised converter poison CAS under a name readers trust.
- A stdout line that is not one of the protocol's own is refused. That is what catches
  launcher chatter (§4.7a, bug 1), and a test with a chatty fake launcher proves it.

**The §4.7a gotchas, handled.**
- **Shutdown.** The child runs its own watchdog at the deadline, so a jailed child exits
  by itself and its launcher tears the jail down. The driver's backstop at deadline +
  10 s closes stdin first, then kills after another 10 s.
- **stderr** is drained on its own thread.

**Require-confinement.** `PRERENDER_REQUIRE_CONFINEMENT=1` refuses three things: running
without a launcher, running networked without fetchd, and a child that the kernel does
not report as jailed. The child checks `security.jail.jailed` itself, so a launcher that
ran it bare is caught. A test with the bare binary as its "launcher" proves the child
refuses. Unconfined runs print `confinement: none — UNCONFINED, for testing`.

**The curl measurement path** (`HttpFetcher`) now runs in the unconfined driver, for
unconfined runs only. It does not apply §2.1's address rules, by design (see its
comment), and the require switch rules it out.

**Measured on macOS, unconfined, serve mode against the old in-child path.**

| | documents | recordings identical | blobs identical |
|---|---|---|---|
| batch 2, offline, exploration on | 51 (50 recorded) | **50/50** | — (no network) |
| batch 2 subset, live network, exploration on | 8 | **8/8** | **393/393** |

The first networked run differed on one Trello image: the new relay cache was cold and
one fetch failed transiently. The URL fetched fine afterwards, and a rerun was
identical.

**Two relay bugs, found by the coverage measurement** (legacy-web §5.4.5.6a), each now a
test:
- **The line bound.** Every child stdout line was held to the 16 KiB URL bound. A report
  line can be longer, and pages ask to fetch 19–179 KB URLs (tripadvisor, walgreens,
  old.reddit), so those documents failed as protocol errors. Now a line is bounded at
  1 MiB, and an over-long URL fails *that fetch* with an `err` frame, without being
  fetched, as the in-process path always let it fail.
- **The exited child.** A child that had already exited (its own deadline) turned the
  next fetch reply into "Broken pipe", a protocol error (theverge, corriere). Now the
  relay stops writing to it, and what it said on stdout decides the outcome.
The FreeBSD run below predates these two fixes: it used the relay as first committed
(68adf2e5), on inputs that hit neither bug.

**FreeBSD (2026-11-04).** This is `scripts/navigator-converter-jail-e2e.sh --rounds 2 --net 1`
run in the cloud VM: 16-CURRENT arm64 under QEMU **TCG**, the broker `navtest` (uid 1001),
one jail per conversion created by portcullisd. **PASS.**

The input is a stand-in for batch 2. This environment's proxy refuses 59 of batch 2's 60
hosts, and the Mac's copy could not be uploaded. So the run used 50 fixed pages:
- `336_node_fs`, the one batch-2 page that fetched;
- 48 of `navigator-render/conformance/`;
- one networked fixture page.

The A/B comparison needs only fixed inputs, which these are.

| stage | result |
|---|---|
| 5 fetchd gate, unjailed | M1 GATE: PASS, all §2.1 rows |
| 9 fetchd gate, in its jail | M1 GATE: PASS |
| 10 require switch, unconfined | refused (`PRERENDER_LAUNCHER`) |
| 11 A/B, 2 rounds × 50 pages, offline, exploration on | recordings 50/50 and reports **byte-identical** in every round (`fails=0`) |
| 12 networked, converter and fetchd both jailed | 1 page: its external script fetched through the jailed fetchd (`fetched=1 failed=0`) and ran; recording identical to the unconfined arm |
| 13 leaks | no jails (live or dying), jaild zombies, mounts or upper dirs |

**Conversion wall time (driver's view, ms):**

| arm | round | p50 | p90 | mean |
|---|---|---|---|---|
| A unconfined | 1 | 787 | 886 | 848 |
| A unconfined | 2 | 786 | 875 | 835 |
| B jailed | 1 | 3799 | 4351 | 3907 |
| B jailed | 2 | 3825 | 4253 | 3906 |

**Jail cost per conversion = B mean − A mean = 3059 ms and 3071 ms, about 3.06 s.**

This is a TCG number, not a product number. The guest is emulated, about 70× slower
than HVF (memory `cloud-freebsd-vm-feasibility`), and jail setup is all kernel work:
nullfs + tmpfs + unionfs mounts, `jail -c`, the exec, and teardown. That work pays the
emulation cost in full. §4.7a's HVF figure, about 25 ms to open and 26 ms to close, is
what to plan with. The TCG number does confirm the cost is per conversion and stable
(two rounds within 12 ms).

**Hard limits, checked in the jail.**
- `procstat -l` on the jailed child shows `cputime` = deadline + 5 s and `vmemoryuse`
  = `PRERENDER_MEM_MB`, both soft and hard.
- `PRERENDER_MEM_MB=256` stops a page that grows a 256 MiB string ("memory
  allocation … failed"). At 1024 the same page converts.
- A broker whose hard limit is already lower makes the child refuse, with EPERM from
  `setrlimit` and exit 3.

**Three harness bugs, fixed in the same commit as these numbers:**
1. The A/B and networked output directories were made by root and written by the
   broker. Every report redirect failed, and the arms "produced no recordings".
2. A corpus with no images has no blob directory, and `diff -r` of two absent
   directories reported "blobs differ".
3. Under TCG, a large `scp` sometimes landed full-size with a ~30 KB run of zeros in
   the middle. The destination hash caught it in 3 of 6 runs. Staging now copies
   again, up to three times, and never runs a binary whose hash does not match.

### 4.7b Fuzzing the hostile-input boundary

§7 requires the validator to be "small, total, and fuzzed with reached-coverage reported".
The harness lives in `tests/fuzz.rs` and is deterministic, seeded and dependency-free — in
the spirit of `scripts/core-fuzz.sh`'s REPLAY half, since a fuzz run you cannot re-run is a
story rather than a test. It runs in the ordinary suite.

**It does not report how many inputs it tried. It reports what it REACHED, and fails if the
set is short.** Every rejection the validator can emit and every note it can raise must have
been produced by a generated input. This project has already been lied to by an execution
count — 27.8M execs at coverage 2, a harness that never reached the code it was aimed at.

That assertion is load-bearing in both directions: a variant the fuzzer never reaches is
either dead code or a blind generator, and a variant that *stops* being reachable after a
refactor fails here instead of silently narrowing coverage.

**It caught its own generator twice, which is the point:**

- `NotAnObject` was never produced. Every seed is a JSON object, and byte-level mutation
  essentially never turns one into a valid non-object. Not dead code — a blind generator.
  Fixed with whole-value replacement.
- `TransitionsTruncated` was never raised, because mutation cannot build 4,097 well-formed
  transitions. Fixed by constructing an oversized table. The honest response to an
  unreachable assertion is to teach the generator, not to stop asking.
- And only 9 transitions ever applied, because the effect-rich seed legitimately fails
  (it removes a node and then operates on it, and apply is all-or-nothing), so the undo path
  was barely entered. A seed that cleanly applies took it to 462.

Under test: `ingest` (untrusted JSON), `Document::accept` (untrusted HTML), `applied` and the
undo path (untrusted effects), and `wire::read_frame` — whose length prefix is written by a
worker that is jailed precisely because it may be compromised.

**The harness itself is verified**: a panic planted in `ingest` is caught at round 11 and the
input printed, so a finding becomes a regression test rather than a rerun.

### 4.7c Measured: what one jail per document costs (M3, open question 1)

§10 q1 made one-jail-per-document conditional on launch being cheap. It is measured now, not
assumed. `jailed_corpus` reports per-request distributions; the harness is
`scripts/navigator-launch-cost.sh` (guest, root). The whole corpus goes through three arms,
**interleaved** A B C × 3 so drift lands on every arm equally:

- **A**: unconfined worker processes (the baseline: spawn + parse, no jail);
- **B**: the jaild one-shot lane called directly by root;
- **C**: the same lane through portcullisd, with the broker as uid 1001.

Each arm counts jids before and after, so it proves its own shape: **0 jails in A, 98 in B
and C, every run**. Release builds throughout (a debug build's open is 73 ms at p50 against
28 ms, so debug numbers answer a different question). FreeBSD 16.0-CURRENT aarch64 under
HVF, 4 vCPU, Laminar, Tessera root.

| ms, p50 / p90 / mean | open | navigate | close |
|---|---|---|---|
| A unconfined | 3.7 / 23.8 / 8.7 | 6.8 / 13.2 / 6.7 | 1.0 / 3.3 / 1.9 |
| B jaild lane, direct | 28.2 / 51.1 / 33.9 | 6.7 / 12.9 / 6.6 | 24.7 / 33.7 / 28.4 |
| C jaild lane via portcullisd | 28.6 / 49.2 / 34.0 | 7.0 / 13.1 / 6.8 | 24.0 / 33.0 / 27.7 |

(Averages of the three runs' per-run statistics, final binaries.)

**Answer: a jail costs ~25 ms to open and ~26 ms to close, per document; keep the default.**
Navigation is unaffected: a pipe round trip adds nothing measurable. The daemon hop is noise
(B ≈ C). Only open is on a reader's path, and 25 ms sits beside a page fetch that is
typically hundreds of milliseconds. The per-site jail-reuse fallback is **not needed** and
is not built. Two caveats bound the claim. It is a VM number and a first measurement, not a
floor. And the p99 open (~80 ms, with rare outliers near 200 ms) has not been decomposed; a
p99 that grows with concurrent sessions would reopen the question.

**Where the 25 ms goes: the vnet, not the jail.** A jail is near-native. What a one-shot
jail pays for is the empty per-app network stack it gets for isolation and MAC hiding
(portcullis.md §9.1c, network.md §0). Measured primitives, averaged over 30–40 rounds:

| primitive | cost |
|---|---|
| fork+exec `/usr/bin/true` (baseline) | 0.4 ms |
| jail create + remove, no vnet | 1.3 ms (mostly two `jail(8)` execs) |
| **jail create + remove, `vnet=new`** | **~70 ms: create 14.5, destroy 54** |
| nullfs / tmpfs / devfs mount + unmount | 1.7 / 1.1 / 1.9 ms |

Work inside the jail is unaffected (navigate: 6.7 ms in every arm).

**Decided (user): not optimised now.** It is a one-time cost per document, in the tens of
milliseconds, and invisible to a reader. It becomes visible only when many jails start or
stop together, such as a burst at init. If that ever matters, the levers are known:

1. No vnet for jails without network access, with the kernel hiding link-layer addresses
   from non-vnet jails.
2. A cheaper vnet teardown, by profiling the 54 ms. It is unmeasured whether Laminar
   contributes; that needs an A/B against ULE.
3. Taking the destroy off any waiting path.

**Found by the measurement, fixed:**

1. **Navigation cost the worker 16 ms p50 (30 ms p90) per step**, in every arm and
   in-process too. The broker's transport was not the cause. The cause was `apply`, which
   built the entire document as a string after every transition *only to take its length*
   for the profile's byte ceiling. That was 64% of an apply; the whole-tree clone is the
   next 19%. Fixed:
   - `Dom::serialized_len()` runs the real serializer into a counting sink, so the count is
     the same code path, not a second serializer to keep in agreement.
   - Escaping is single-pass with no allocation. The four chained `replace`s built four
     strings per text node.

   Result: 6.8 ms p50 (2.4×), and the corpus completes in 4.4 s against 10.5 s. Every
   serialized state of the corpus (616 documents: base, each applied transition, each
   rewind) is **byte-identical** to the old serializer, and the diff was positive-controlled
   with a planted byte.
2. **Every close stalled the broker ≥ 20 ms.** Retirement slept a fixed 20 ms before its
   first check on the worker, and the broker is single-threaded, so each close blocked
   every session. Now a backoff from 1 ms, capped at 5 ms. Unconfined close: 20.0 → 1.0 ms
   p50. Jailed close falls to its real teardown time (~24 ms). Teardown still runs
   synchronously in the broker; moving it off the request path is the remaining step.
3. **The direct lane gave a non-root caller the wrong reason.** It failed with
   `host identity: Permission denied`, reading the root-only identity secret, before it
   reached anything that said "this lane needs root". It now refuses first, and names
   `--daemon`.

### 4.5 Recording format version 2

`atrium-navigator-recording/2` adds one field: an insert's `index`, the position the markup
occupies among its parent's children.

**It is not there for reversibility** (§4.4 gets that from the document) **but for
fidelity.** Version 1 recorded only the parent, so a replay could do nothing but append: a
row the page inserted into the middle of a list came back at the end, and no error reported
it. Real corpus recordings carry indices of 51 and 3 — inserts that version 1 silently moved.

Position is the one thing here that genuinely cannot be derived, because replay chooses
where to put the markup. That is the test a new field has to pass.

Version 1 is still accepted: recordings live in a content-addressed store and do not
disappear when the producer moves on. A version 1 insert carries `index: None` — **absent,
not defaulted to a plausible zero** — so it appends, and a consumer can tell which guarantee
it is getting. An out-of-range index clamps rather than refusing, degrading to the version 1
behaviour instead of dropping content.

The version was bumped rather than the field added compatibly because a consumer that read a
version 1 recording and assumed the field was merely missing would produce a document
differing from the recorded one, with nothing to signal it.

**The format is closed under inversion.** Every effect kind's inverse is expressible in it —
`insert` inverts to `remove-range` (take `count` nodes back out at `index`), which inverts
back to `insert`. That is what lets an undo be an ordinary transition, validated and applied
by the ordinary path, rather than a second and less examined mechanism.

### 4.5a Recording format version 4: templates (2026-11-05)

`atrium-navigator-recording/4` adds `templates`: one transition recorded once over the
sibling elements whose handler it is (legacy-web §5.4.5.6c). Version 3 added only what
the normalizer reads (`stylesheets`, `images`), so it needed no section here.

```
"templates": [
  { "event": "click", "attribute_only": true,
    "effects": [ { "kind": "attribute", "target": "$t", "name": "aria-selected", "from": null, "to": "true" },
                 { "kind": "attribute", "target": "$0", "name": "class", "from": "panel", "to": "panel shown" } ],
    "instances": [ { "trigger": "html:0>body:0>ul:0>li:0>a:0", "anchored": true, "slots": ["html:0>body:0>div:0>div:0"] },
                   { "trigger": "html:0>body:0>ul:0>li:1>a:0", "anchored": true, "slots": ["html:0>body:0>div:0>div:1"] } ] } ]
```

An effect's address is a literal path (the same node for every instance), `$t` (the
instance's trigger) or `$N` (the instance's slot N). Values (`from`, `to`, `html`,
`index`) are the same for every instance. That is what makes it one transition.

- **Expanded at ingest, never carried.** Each instance becomes the ordinary `Transition`
  it stands for, so apply, inversion, sessions and restore see the vocabulary they
  always have. A template is a compression of the recording, not a new behaviour, and
  there is no second replay path to examine.
- **Refused:** a `$` address that is neither `$t` nor a slot number below
  `max_effects_per_transition`, and an instance whose slot count differs from the number
  the effects name. So is a `/3` recording that carries `templates`: a `/3` reader would
  have ignored the field and replayed a subset, with nothing to say so.
- **Bounded:** at most `max_templates` (256), and the instances count against
  `max_transitions` once expanded. The expanded effects count against `max_total_bytes`:
  a few bytes of template must not stand for more than a flat recording could say.
  Instances past either bound are dropped, and `Note::TemplateInstancesTruncated` says how
  many.
- **Written only when there is one.** A recording with no template is `/3`, byte for byte
  what it was. So the non-exploring control arm, and every recording without siblings to
  generalise, is unchanged.

Tests: `tests/templates.rs` (expansion, replay and back, refusals, bounds) and the
fuzzer, which has a `/4` seed and must reach `TemplateInstancesTruncated`.

---

### 4.8 Big documents: bounded memory and smooth scrolling (proposed 2026-11-05)

*Asked by the user, 2026-11-05: "memory management for pages with or without js or other
scripting that are super big — we don't want an OOM. Scrolling a 1000 page document
shouldn't choke the navigator too or feel laggy." Steps 1–2 and step 3 phases 1–4 built
(below: bands, table rows, the wire and far jumps); the image working set not yet.*

**Today, a big page is safe, but by refusal.** Checked against the tree on 2026-11-05:

| stage | what bounds it today | what scales with document size |
|---|---|---|
| converter | `RLIMIT_AS` and CPU (§4.7d); a 128 MiB byte budget per conversion; jaild rctl `memoryuse` | the DOM and the JS heap |
| profile | refuses a document over **8 MiB**, **131,072 elements** or depth 256 (`navigator-dom` `profile.rs`) | n/a: a bigger document never reaches the reader |
| worker | one process per document; jail limits | **all of it**: the whole DOM, the whole layout, and **one scene for the whole page** (`nsg` emits `viewport w h` with h the full extent), plus every image decoded, up to 256 MiB |
| broker | 16 sessions and 64 MiB of session state (§4.4a) | a whole-tree copy per applied transition (§4.3) |
| Fresco | the window's scene buffer | the whole page's scene |

So the machine is not at risk: every process that touches a page is jailed with a hard
memory limit, and the profile turns a huge page away. But the cost is that a 1000-page
document is refused, or, just under the ceilings, it is laid out, painted and decoded in
full before anything shows. Memory then grows with the document instead of with the
screen.

**The goal.** Memory and per-frame work follow **what is on screen**, plus a margin, not
what is in the document. A 10-page and a 1000-page document cost about the same to scroll.
The ceilings can then rise to what we can measure, not what fits whole.

**The design: six parts.**

1. **Lay out progressively; paint in bands.**
   - The worker lays out the first screen first and publishes it. That is the same shape as
     progressive delivery (§5.4.5.7 of the legacy-web spec).
   - It then lays out the rest in the background, in block-flow order. The scroll extent
     is an estimate until layout finishes, and the scrollbar settles once.
   - Layout is kept as compact geometry: boxes and line runs. A display list is not kept
     for the whole page.
   - **Checkpoints.** At each band boundary, which falls on a top-level block boundary, the
     worker saves a small checkpoint: the y position, inherited state and counters. Any band
     can then be laid out again from its checkpoint without redoing what lies above it.
     Layout is deterministic, so a band laid out again is byte-identical, and a jump to page
     700 lays out from the nearest checkpoint.
   - The scene is cut into **bands** of about one viewport height each. The worker emits a
     band's scene only when asked for it.
2. **The UI keeps a window of bands.**
   - The UI holds the bands for the screen plus about two screens either way, and more
     ahead in the direction of a fling.
   - Bands far from the viewport are dropped from Fresco and from the worker's caches;
     their geometry stays.
   - **Scrolling never waits on the worker.** Fresco moves bands it already has. A band
     that isn't ready yet is drawn as its background colour, with text first where there
     is time, never as a frozen frame.
3. **Images form a working set, not a document total.**
   - Header sizes (cut A) give layout without decoding.
   - Pixels are decoded only for bands in the window and evicted as they leave it.
   - The 256 MiB ceiling becomes a working-set ceiling, so a photo-heavy 1000-page document
     can still show every image as the reader reaches it.
4. **Find, links and accessibility read the compact layout, never the scene.** So they work
   across the whole document without painting it.
5. **Scripted and endless pages are bounded by recording, not by luck.**
   - The converter's JS heap stays under its hard limit. The exploration budget already
     limits how many "load more" steps a recording holds.
   - At view time an endless feed is a data binding (legacy-web §5.4.5.1). The document
     lane owns the layout, so it can keep a window of items and unload the oldest. That's
     list virtualisation, and the page doesn't have to do it itself.
   - A tier-4 island has its own jail limit. When it hits the limit, the island is replaced
     by a "stopped, reload" panel, and the rest of the page is untouched.
6. **The Navigator answers memory pressure** through the memory-pressure trim channel
   ([atrium-memory-pressure.md](atrium-memory-pressure.md) §4). The cheapest step comes
   first:
   - off-screen bands;
   - decoded images outside the window;
   - undo states beyond the current one;
   - then background pages' workers. A background page keeps its recording and scroll
     anchor and re-renders when shown, at about 25 ms for the jail (§4.7c) plus layout.

**Hard limits stay, as the last line.** Each process keeps its jail limit. Exceeding it
ends that page's process, never the machine, and the reader keeps what was already
delivered, as with a timeout (legacy-web §5.4.5.7). Once the working set is bounded, the
profile ceilings can be raised by measurement.

**Measure first, then build.** The test set:
- the HTML Living Standard as one page (about 13 MB);
- a 1000-page book from Project Gutenberg;
- a table of 100,000 rows;
- a forum thread with thousands of comments;
- an endless feed;
- a photo-heavy gallery.

For each, measure:
- peak memory per process, against document size (the target is flat past a few screens);
- time to first display;
- frame time while flinging, p50 and p99, at 60 and 120 Hz;
- time to reach the end with a single jump.

**Measured (2026-11-06).** The test set, plus the GNU coreutils manual, through today's
pipeline (e23e712d). Inputs are in `~/src/navigator-corpus/bigdocs-2026-11-06/`, outside
the repo; two of them are synthetic, generated there.
- *Converter.* The driver as in the time-to-first-display arms: online through fetchd,
  progressive, with exploration. The converter child ran under `/usr/bin/time -l`, which
  gives its peak resident set.
- *Worker.* `examples/bigdoc_measure` ran the display path (ingest, normalize, layout,
  scene) as the per-document worker does, one process per stage, so each stage's peak is
  its own. These are the readable recordings; the first artifacts measure within a few
  percent of them.

| document | HTML | elements | profile ceiling | converter peak | worker peak (ingest → normalize → layout + scene) | normalize / layout / scene ms | first display from arrival | scene: runs · extent · NSG | whole page as RGBA |
|---|---|---|---|---|---|---|---|---|---|
| HTML Living Standard (one page) | 16.0 MB | 339,742 | **bytes and elements** | 957 MiB (47 s) | 671 → 3,941 → 4,007 MiB | 3,802 / 2,549 / 214 | 8.8 s | 794k runs · 2.64M px · 78 MB | 7.9 GiB |
| 100,000-row table (synthetic) | 7.1 MB | 500,015 | **elements** | 440 MiB | 463 → 5,180 → 5,469 MiB | 5,643 / 4,730 / 116 | 10.4 s | 400k runs · 2.80M px · 42 MB | 8.3 GiB |
| HN thread, 2,540 comments | 3.5 MB | 78,585 | — | 227 MiB | 155 → 1,166 → 1,256 MiB | 4,105 / 1,152 / 35 | 6.6 s | 125k runs · 320k px · 12 MB | 975 MiB |
| coreutils manual | 2.1 MB | 48,322 | — | 90 MiB | 107 → 692 → 742 MiB | 1,617 / 457 / 36 | 4.4 s | 136k runs · 537k px · 13 MB | 1.6 GiB |
| War and Peace (Gutenberg, about 1,400 printed pages) | 3.8 MB | 14,941 | — | 81 MiB | 123 → 242 → 410 MiB | 281 / 715 / 139 | 1.3 s | 573k runs · 1.43M px · 49 MB | 4.3 GiB |
| photo gallery (Commons, 111 images) | 0.15 MB | 1,195 | — | 32 MiB | 19 → 52 → 53 MiB | 97 / 15 / 0 | 1.1 s | 397 runs · 2.6k px · 79 kB | 8 MiB |
| endless feed (synthetic: "Load more" never ends) | 4 kB | 29 | — | 24 MiB | 16 → 18 → 18 MiB | 1 / 1 / 0 | 5 ms | 663 runs · 1.1k px · 53 kB | 3 MiB |

*What it says.*
- **Worker memory grows with the document, and steeply.** Peak resident memory is about
  10–15 KiB per element: HN 1.2 GiB for 3.5 MB of HTML, the table 5.5 GiB. A worker jail
  with a 4 GiB limit would end the table and the HTML standard, but they are refused by the
  profile first.
- **Where it goes** (peak RSS after each normalize phase, HN / table):
  - cascade, resolve and collapse add 340 / 560 MiB together;
  - **table pre-measure adds 665 MiB / 4.1 GiB, the largest single step.** HN lays its
    comments out with tables.

  The likeliest cause: the renderer's cascade keeps a full computed-style vector (every
  longhand) for every element, in pre-measure and again in layout. A normalized document
  has one class per distinct block, so most elements' computed styles are identical.
  **Sharing computed styles** between identical elements, as browsers do, is the first
  memory fix to try. It is independent of bands, and cheaper.
- **The profile refusal works as the last line.** The two documents over its ceilings are
  converted, and the broker refuses them before any reader sees them. Raising the ceilings
  (decision 1) must wait for style sharing and bands. Today, admitting the table would cost
  a 5.5 GiB worker.
- **The scene is whole-page** (`viewport w h` with h the full extent). The scene for War
  and Peace is 49 MB of NSG, 573,000 runs over 1.43 million px (about 2,400 screens). As
  one RGBA surface it would be 4.3 GiB. Fresco holds the scene, not pixels, so the
  surface figure is the bound that bands make unnecessary, not today's cost.
- **Time to first display** for the big documents is display-bound: 4.4–10.4 s from
  arrival, almost all of it normalize and layout of the whole document. A first band
  would need only the first screen's share of that.
- **Script and endless pages are bounded by recording.** The endless feed's exploration
  recorded one "Load more" (50 items) and stopped. The converter peaked at 24 MiB, and
  the HTML standard's at 957 MiB, under its jail limits.
- **Not compared: the first display versus later scripted versions on these documents.**
  Their first and readable artifacts measure the same within a few percent.
- **Frame time while scrolling: no harness exists.** Fresco accepts scroll input
  (`InputPointerScroll`), but no Navigator UI drives scrolling yet, so the frame-time row
  of the test set (p50/p99 at 60 and 120 Hz, time to jump to the end) cannot be measured
  today. Because the whole scene is resident, a jump costs no layout today. The price of
  that is the memory above. The harness is a prerequisite for building bands, so their
  cost and benefit can be measured.
- Cut B (e23e712d) is not the cause. The same tool built at 0beab757 gives the same
  normalize peaks within 1% (coreutils 690 vs 697 MiB, HN 1,159 vs 1,166 MiB), with
  normalize 2.1–2.4× slower.

**Order of work, approved by the user on 2026-11-06.** It follows SVG, page zoom (with the
two table items) and the float and border-collapse layout items:
1. computed-style sharing in the renderer and normalizer: output byte-identical, with
   worker peak memory and time measured on this set before and after;
2. a scroll harness that drives Fresco scrolling and reports frame time p50 and p99 at
   60 and 120 Hz;
3. bands and checkpoints, as above;
4. only then, raise the profile ceilings, by measurement.

Data: `~/src/navigator-corpus/bigdocs-2026-11-06/`. `convert.sh` produces `convert.txt`
and `timing.tsv`; `worker.sh` produces `worker.tsv`; both tool builds are kept beside them.

**Step 1 built: computed-style sharing (2026-11-06).** The renderer's cascade
(`navigator-style`, used by layout and by the normalizer's table pre-measure) now shares
one `Rc<Style>` between elements whose computed style must be equal.
- *The key.* The parent's shared style (its address) and the declarations that apply, in
  cascade order (layer, specificity, source position). The environment and the root font
  size are the same for every element after the root.
- Diagnostics produced while computing a style are reported again for each element that
  shares it, so the output does not change.
- An element no longer clones its parent's whole style to read from it.

*Byte-identical.* All 421 identity documents give the same normalized documents and NSG.
Every suite passes, and conformance is 64/64. Scene node counts are equal in every run
below.

*Measured* (`examples/bigdoc_measure`, one process per stage, the readable recordings,
3 interleaved runs per arm, medians; before = 20ab1993):

| document | worker peak, normalize stage | worker peak, layout + scene | normalize ms | layout ms |
|---|---|---|---|---|
| 100,000-row table | 5,174 → **1,091 MiB (−79%)** | 5,468 → **1,320 MiB (−76%)** | 5,366 → 3,702 | 4,675 → 3,095 |
| HTML Living Standard | 3,932 → **1,206 MiB (−69%)** | 4,032 → **1,251 MiB (−69%)** | 3,720 → 2,716 | 2,512 → 1,594 |
| HN thread | 1,133 → 500 MiB (−56%) | 1,262 → 613 MiB (−51%) | 3,934 → 3,646 | 1,145 → 865 |
| coreutils manual | 699 → 318 MiB (−54%) | 742 → 361 MiB (−51%) | 1,625 → 1,484 | 449 → 304 |
| War and Peace | 244 → 156 MiB (−36%) | 410 → 290 MiB (−29%) | 282 → 239 | 718 → 674 |
| photo gallery | 54 → 48 MiB | 54 → 48 MiB | 113 → 108 | 15 → 13 |
| endless feed | 18 → 17 MiB | 18 → 18 MiB | 1 → 1 | 1 → 1 |

Peak memory per element falls from 10–15 KiB to about 2.5–8 KiB: the table holds 1.3 GiB
for 500,000 elements. What remains is mostly the normalizer's own per-element
declaration maps and the layout itself. Those are the next to share, and bands bound the
layout.

**Step 2 built: the scroll harness (2026-11-06)** (`navigator-render/examples/scroll_bench`).

*Asked by the user, 2026-11-06:* check short-term jitter and long-term lag too, because
"sometimes one scroll lags for a few seconds and then catches up. Average won't catch
this." So the harness reports the tail and the timeline, not only a distribution.

*How it measures.* A replay on a virtual clock with real costs.
- Each vsync samples a scripted scroll target:
  - `fling`: 4,000 px/s decaying over 0.6 s;
  - `flings`: one every 1.5 s;
  - `page`: page-down every 250 ms;
  - `jump`: to the end at 1 s and back at 4 s.
- The compositor's per-frame work is actually run, and its measured time advances the
  clock:
  - cull the scene to the viewport;
  - translate it into a Fresco frame: rects, filled paths, text through an R8 glyph atlas
    rasterised on first sight, images as textures;
  - `Frame::pack_report`, as frescod packs a window;
  - raster on frescod's CPU kernels (Tier-2).
- A frame presents at the first vsync after its work ends. Until then the previous one
  stays up.
- Not modelled: the GPU path, the socket, clips, groups and transforms (drawn unclipped),
  and image decode arrival.

*What it reports*, per document, rate (60 or 120 Hz) and pattern:
- *Jitter:* frame time p50, p99, p99.9 and max; the change between consecutive frames
  (p99); missed vsyncs (a vsync with nothing new while the target moved); the longest run
  of them.
- *Lag:* the age of what is shown (how long ago the displayed position was the target),
  worst and as the worst one-second window; the position error in px.
- *Stalls:* every stretch where what is shown is more than 100 ms old, with its start,
  length, worst age and catch-up time; counted over 100 ms, 500 ms and 1 s.
- `--csv`: one row per vsync (`t_ms target_y shown_y age_ms cost_ms missed`), the
  timeline.

*Positive control.* `--inject-stall 2500@2000` makes one frame 2.5 s slower. The harness
reports exactly one stall: start 2,100 ms, length 2,433 ms, worst age 2,517 ms, caught up
in 17 ms. The frame-time-change p99 stays at 0.99 ms, so a distribution alone would not
have shown it.

*Baseline* (today's whole-page scene; 7 documents × 2 rates × 4 patterns × 2 runs, 112
runs, 8 s each, 800 × 600 at 1×; M4 Max):
- **60 Hz:** every document holds the rate.
  - Frame p50 7.5–13.6 ms, p99 8.6–16.6 ms.
  - A rare single spike to 22–32 ms (one missed frame, shown age up to 50 ms) appears in
    p99.9 and max only.
  - **0 stalls over 100 ms** in all 56 runs.
- **120 Hz:** flings miss 35–50% of vsyncs on the documents that scroll far, so they run
  at about 60 fps.
  - A frame costs 8–12 ms whatever the document: War and Peace's 573,000 nodes cost
    7.7 ms, the 663-node feed 12 ms. So the limit is the full-frame CPU raster at
    800 × 600, not the document or the per-frame walk of the whole scene.
  - A compositor that redraws only what a scroll exposes (Fresco's damage tracking), or
    the GPU path, is what 120 Hz needs. The harness redraws every frame in full.
- **No long lags today, by construction.** The whole scene is resident, so scrolling
  never waits on the worker. The price is the whole-document layout before anything
  shows: 0.3–3.3 s here, the 100,000-row table the longest. It is also the memory above.
  Bands remove that cost. They introduce worker latency while scrolling, and that is what
  this harness's stall detector is for.

**The compositor for scrolling: shift, draw the strip, greek a fling (proposed
2026-11-06).** *Asked by the user:* for flings, redraw only the newly exposed strips, but
still give the impression of scrolling, perhaps with blurred text. Measured in the harness
(`--compose strip`, `--blur`), and proposed to whoever owns frescod's damage and upload
paths.
1. *Shift, then draw only the strip.*
   - The window keeps its last frame. A scroll moves those pixels by the whole-pixel delta
     (a GPU blit or texture offset) and rasters only the rows it exposed.
   - A delta as large as the window, or the first frame, redraws everything.
   - `--verify` checks that the composed window equals a full redraw at the same position:
     **0 differing frames** in every run checked. A control that skips writing the strip
     differs in 239 of 240.
2. *A fast fling is drawn as motion.*
   - Above 2,500 px/s, new strips draw text as one bar per run (greeked), at half the text
     colour's alpha.
   - With blur on, the window gets a vertical motion blur scaled to the velocity: 2 px per
     1,000 px/s, at most 12. That is one GPU pass; the CPU here only stands in for it.
3. *Sharpened progressively when it slows.*
   - The window drawn greeked is queued in 12 slices of page rows, and one is re-rastered
     sharp per frame. That is sharp within 100 ms at 120 Hz.
   - One full redraw at that moment missed a vsync. Thirds missed 2% and sixths 3% on the
     HTML standard: every frame also pays about 3 ms to walk its 935,000-node scene, so a
     100-row slice cost 9 ms. Bands, with a spatial index, remove that walk.
4. *The same trick hides band latency* (with bands): a band not yet laid out when a fling
   reaches it is drawn as greeked lines in the page's text colour, not blank and not
   frozen. The stall detector holds real content to 100 ms after the fling stops.

*Measured at 120 Hz, `flings`, 8 s, 2 runs each* (frame p50 / p99 ms; missed vsyncs):

| document | full redraw (today) | strip | strip + greek + blur, sharpened in 12 slices |
|---|---|---|---|
| HTML Living Standard | 9.9–10.4 / 12.4–12.5; **50%** | 3.6–3.8 / 4.0–4.1; 0.1% | 3.7 / 8.0; 0.1% |
| 100,000-row table | 8.6–8.7 / 9.9–10.1; **50%** | 2.8–2.9 / 3.1–3.2; 0.1% | 2.8–2.9 / 6.2; 0.1% |
| HN thread | 10.4–11.2 / 13.1–15.4; **50%** | 2.6 / 2.9–3.0; 0.1% | 2.6 / 5.8; 0.1% |
| War and Peace | 7.7–7.8 / 8.7; 5–9% | 3.0–3.1 / 3.4–3.5; 0.1% | 3.0 / 6.6; 0.1% |

- The one missed vsync is the first frame, which draws the whole window.
- Every run: shown age at most 17 ms (two frames), 0 stalls.
- About 14–18 rows are rastered per frame, of a 600-row window.
- *Page-down* still misses 3%. A 600 px jump exposes the whole window, so a page-down
  should be animated over a few frames, as browsers do, which makes it strips too.

**Step 3, phase 1 built: the first screen, and a banded scene (2026-11-06).** It is
placed in the layout crate (`navigator-render`) to serve the contract of pergola.md §10.3.
A `DocumentView`, the Navigator worker and any long list use it alike.
- *`render_html_first(…, y_limit)`.* The layout of the document down to the limit. It
  stops at the first block in normal flow whose top is below the limit, and a table in
  normal flow stops measuring rows once they pass it.
  - The stop is off inside flex and grid containers, table cells and trial measurements,
    whose sizes must stay exact. It fires only before a block with no inline content
    pending: the profile's total-fit line breaking makes a line depend on the words
    after it.
  - After the stop, a `position: fixed` child is still laid out (doc28's header is
    written at the end of its document).
  - If anything skipped has a transform or is absolutely positioned, the first screen is
    unsafe, and the page is laid out whole. ynet's carousel cards are translated up from
    below the cut. `first_screens_laid_out_whole()` counts these.
- *`bands::index(scene, band_h)`.* Each draw-order entry goes into every band its painted
  extent touches (through its transform). `nodes_in(y0, y1)` returns a viewport's nodes;
  `band_scene(b)` returns a self-contained scene with the scene's tables. Bands do not yet
  carry vector-SVG bitmap fallbacks, whose path indices change.

*Checked.* `tests/bands.rs`, compared by pixels:
- the first screen draws what the full layout draws there, and a planted recoloured box
  is caught;
- a 1,000-row table stops at about 40 rows;
- a page that fits is byte-identical;
- every band of a page draws its rows as the full scene does.

On the corpus (`examples/first_screen_check`: first screen 600 px, laid out to 1,200 px,
8-bit pixels):
- *Result.* All 122 readable documents and the 7 big ones draw their first screen
  identically to the full layout.
- *The search that got there:*
  - The first version differed on 7 pages: a fixed header at the end of a document, a
    paragraph split mid-content, and transformed content from below.
  - The remaining two were real and are fixed: wikihow's opacity groups composite
    differently once more content is in the draw order (now laid out whole), and ynet's
    carousel.
  - ynet differed by up to 227 levels before the transform rule.
- *Coverage.* 36 of the 122 readable pages take the fast path; the rest have a
  transformed or absolutely positioned element below the fold and are laid out whole, as
  before.
- *The identity set* is byte-identical with the limit unset.

*Measured* (layout and scene, single runs):

| document | whole layout | first screen | nodes |
|---|---|---|---|
| War and Peace | 693 ms | **25 ms** | 573,326 → 157 |
| 100,000-row table | 3,286 ms | **394 ms** (parse and cascade of the whole document remain) | 400,005 → 161 |
| HTML Living Standard | 1,599 ms | **480 ms** | 934,835 → 262 |
| coreutils manual | 312 ms | 84 ms | 154,620 → 217 |
| HN thread | 871 ms | 883 ms: no gain (its comments are one table cell, where the stop is off) | — |
| readable corpus, the 36 cut | p50 18 / p90 147 ms | p50 10 / p90 54 ms | — |

- *Not yet faster:* normalize still processes the whole document (3.7 s for the table,
  2.7 s for the standard), before any layout. **Correction (same day):** this section
  first said the normalized document is cached by the recording's address, so a second
  visit skips it. It is not. No such cache exists. The display path (normalize, layout,
  scene) is not wired into the broker's worker yet; `bigdoc_measure` and these examples
  model it. The cache keyed by recording and font-set version (§8.5) is a design.
  (Since built: phase 1b, below.)
- *Where a big document's worker memory is now* (after style sharing; peak resident set,
  `examples/bigdoc_measure` and `examples/layout_peak`):

  | document | normalize + layout + scene (today's path) | from an already-normalized document | …first screen only |
  |---|---|---|---|
  | HTML Living Standard | 1,251 MiB | 646 MiB | 409 MiB |
  | 100,000-row table | 1,320 MiB | 669 MiB | 344 MiB |
  | HN thread | 613 MiB | 215 MiB | 218 MiB |
  | coreutils manual | 361 MiB | 120 MiB | 79 MiB |
  | War and Peace | 290 MiB | 200 MiB | 51 MiB |

  Normalizing is about half the peak of the big documents. A worker that receives the
  normalized document (normalized once, stored by content address) halves its peak and
  skips 2.7–3.7 s. Bands with checkpoints then remove most of the layout's share. What
  remains is parsing and styling the whole document.
- *Band culling in the compositor* (`scroll_bench --bands`): the index takes 7–14 ms to
  build. On the HTML standard at 120 Hz with strips, frame p50 goes 4.0 → 2.3 ms and p99
  8.9 → 5.2 ms. `--verify`, against a reference that walks the whole scene, finds 0
  differing frames over flings, page-down and jump on four documents. It caught a
  harness bug on the way: a full redraw at a fractional scroll position, drawn half a
  pixel off.

**Step 3, phase 1b built: the display path in the worker, normalized once (2026-11-06).**
This replaces the correction above. The display path is now in the broker's worker, and
the normalized document is stored by content address.
- *The request.* `Request::Scene { session, limit_px }` gets `Event::Scene { nsg,
  from_cache }`: the session's first screen as NSG, down to `limit_px` (1..=8192). On the
  wire it is `SCN`. The payload is `<initial> <limit_px> <cached_len>\n` plus the cached
  document. The answer is `<normalized_len>\n` plus the normalized document plus the NSG.
  The worker runs `display::first_screen`: `render_html_first`, then exactly the nodes
  `bands::index(..).nodes_in(0, limit)` returns. An answer is therefore bounded by a
  screen, not by the document.
- *Normalized once.* A session's document is the recording's *initial state* until a
  Navigate, Forward, Restore or Swap succeeds on it. While it is:
  - the broker keys the recording by the BLAKE3 hash of the exact bytes that worker
    processed;
  - the first worker to show it normalizes and hands the normalized document back with
    the scene;
  - the broker stores the document in its blob store;
  - every later worker showing that recording gets the document and only lays out.
  
  A navigated document is normalized again from its current serialization, and it is
  never read from or written to the cache. The cache adds no capability. A worker can
  only poison the entry for the recording it was given, which its own answers already
  render. The broker checks every answer, `check_scene`:
  - the NSG is at most 8 MiB and has an `nsg 0.` header;
  - the normalized document is at most 16 MiB and is UTF-8.
  
  A jailed worker whose answer is malformed is retired.
  The index from recording to normalized document lives in the broker's memory, so it
  cannot outlive the binaries (and font set) that produced its entries. An index that
  persists across restarts must add the font-set and normalizer versions to the key
  (§8.5).
- *Checked.* `tests/display.rs`, through the in-process and jailed hosts:
  - the first scene normalizes (it applies the recording's stylesheet, and a recoloured
    expectation fails);
  - a second session on the same recording is served from the cache with identical NSG;
  - a navigated session is not served from the cache and draws the revealed content;
  - an out-of-range limit is refused and the session survives;
  - a cached document that differs from the recording is what gets drawn, which proves
    the cached arm lays out the stored document;
  - malformed answers are refused.
  
  On the 6 big documents the NSG is byte-identical across two normalizing runs and the
  cached run. No normalizer or renderer code changed, so the identity set is unaffected
  by construction.
- *Measured* (`examples/scene_peak`, one process per run, 3 runs per arm, limit 1,200 px;
  the spread across runs is under 6%):

  | document | normalizing (first worker) | from the stored normalized document |
  |---|---|---|
  | HTML Living Standard | 3,709–3,947 ms, 1,145 MiB | **659–665 ms, 720 MiB** |
  | 100,000-row table | 4,715–4,857 ms, 1,157 MiB | **452–461 ms, 476 MiB** |
  | HN thread | 5,301–5,514 ms, 611 MiB | **1,036–1,089 ms, 228 MiB** |
  | coreutils manual | 1,846–1,873 ms, 319 MiB | **117–123 ms, 117 MiB** |
  | War and Peace | 291–298 ms, 155 MiB | **56–58 ms, 135 MiB** |
  | Commons gallery | 164–165 ms, 48 MiB | **32 ms, 27 MiB** |

  - *What remains on the cached path.* The worker still parsed the whole recording JSON
    for the normalizer's inputs, which a cached document no longer needs: the standard's
    recording is 16 MiB. **Since done (phase 2, 7ec529da):** `display::first_screen` parses
    the recording only when it normalizes; a stored document needs only the image sizes,
    which are read line by line. Cached arm, 3 runs each (`scene_peak`): HTML standard
    720 → **441 MiB**, 100,000-row table 479 → **361 MiB**, War and Peace 137 → **59 MiB**,
    coreutils 118 → **84 MiB**, HN 228 → 222 MiB. (Opening a session still ingests the
    recording, in its own step.)
  - *Parsing and styling the whole document* remain (phase 2 below bounds the layout).

**Step 3, phase 2 built: checkpoints, bands laid out on their own, and the content source
(2026-11-06).** In `navigator-render`, so a `DocumentView` and the worker use it alike
(pergola.md §10.3–§10.4).
- *Styled once, laid out band by band.* `html::prepare` parses and styles the document once.
  `bands::Banded` then lays it out one band at a time: the first time in document order, each
  band from where the previous one stopped (the first pass), and again from the band's own
  checkpoint whenever an evicted band is wanted back.
- *Checkpoints.* A band is cut at the first block boundary below its height (one viewport),
  and only at boundaries in plain block flow: never inside flex, grid, a table, a trial
  measurement, a line box or an out-of-flow box, and never with inline content pending.
  The checkpoint records the block, its y and the vector-SVG share left there. A band run:
  - skips every block an earlier band completed, by its recorded outer height (margins never
    collapse and there are no floats, profile §3.1, so the next block starts exactly there),
    and the line boxes flushed before a block it continues;
  - starts a box it continues (an ancestor of its checkpoint block) at the child on the way
    to the checkpoint, at the y recorded for it, without walking the siblings before it;
  - stops before the next band's checkpoint block.
  Layout is deterministic, so a band laid out again is identical (tested, NSG byte for byte).
- *Boxes that span bands* (a section's background, a wrapper's border) are painted by every
  band they span, at their final size once a band has completed them. A band that cut a box
  before its size was known painted nothing for it and is marked stale; it is laid out again
  before it is shown (and before the overlay settles, when the box positions or transforms
  what it holds).
- *Leaves a band paints outside itself* (positioned and transformed boxes, shadows, a fixed
  header written last) are keyed in the whole document's painting order: stacking key, then
  (the band that emitted it, its emission index), a spanning box's paint keyed where the box
  began. For each other resident band it reaches, such a leaf either goes into that band's
  group at its place (the whole layout paints it under something there: a positioned note
  written above a section whose background covers it) or into the **overlay group** after
  the bands (it paints over everything there), clipped to that band. The overlay holds
  entries for resident bands only, so it is bounded by the window too.
- *One shaping cache* (per word) is shared by a document's band runs, bounded at 200,000
  words; without it each band shaped cold and the first pass took twice as long.
- *The content source* (`source::DocumentSource`, the `ContentSource` trait): `want {range,
  zoom, prefetch}` in; `extent {height, settled}`, `band_ready`, `band_updated`,
  `band_evicted` out; one scene group per band and the overlay group written through a
  `SceneSink`; only the wanted range plus prefetch resident; idle work runs the first pass
  in the background; a new zoom lays the document out again (§4.9). The extent is an
  estimate (laid-out height over the fraction of the document's nodes reached) until the
  first pass completes, then exact.

*Checked.*
- `tests/checkpoints.rs`, by pixels against the whole layout, with a planted-difference
  control: every band of a page with spanning sections, lists, flex cards, a lifted box
  with a shadow, an absolutely positioned note and a fixed header written last, at band
  heights down to 60 px; a band laid out again is identical to its first run; list markers
  survive a cut before an item's first line (fails with the old rule). `tests/source.rs`:
  resident bands stay within want + prefetch while the whole document scrolls; a jump to
  the end settles the extent; idle work settles without making anything resident; a new
  zoom lays out again.
- *The oracle on real documents* (`examples/band_check`, bands of 600 px; each band laid
  out again from its checkpoint, drawn as a host draws it — its group, then the overlay's
  leaves for it — and its whole pixel rows compared with the whole layout's at 8 bits):
  - 129 corpus documents, 3,234 bands, 1,415 checked (at most 40 per document, evenly
    spaced): **0 differ**; no document fell back to a whole layout.
  - The 7 big documents, 6,403 bands, 127 checked: **0 differ**.
  - Whole-layout NSG byte-identical before and after on all 136.
- *What the oracle found on the way* (each fixed, each in the code's comments): leaves
  under a transform that scales to zero (paint nothing; in no band); inline content
  before a block a band continues, laid out again by every band (pubmed's skip link); a
  block's recorded height counting the line boxes before it twice (nhs.uk, 48 px low); a
  box whose first child holds the cut not seen as spanning, so drawn twice (figma, folha)
  — the exact test is "an ancestor of the checkpoint block"; a list marker cleared when the
  band that placed it was laid out again (the HTML standard's step "7.").

*Measured* (`examples/band_peak`, one process per arm, 3 runs per arm, from the stored
normalized document, so parsing and styling are in both arms; the source with bands of
600 px and one viewport of prefetch; M4 Max):

| document | bands | whole layout: ms, peak | first screen: ms, peak | first pass to the end: ms, peak | scrolled end to end: peak, resident | a jump, from a checkpoint |
|---|---|---|---|---|---|---|
| HTML Living Standard | 3,723 | 2,029–2,303 ms, 639–645 MiB | **576–601 ms, 409–410 MiB** | 2,082–2,372 ms, 468–472 MiB | **470–473 MiB**, 4 bands | 1–2 ms |
| War and Peace | 1,979 | 914–992 ms, 201–203 MiB | **31–33 ms, 51 MiB** | 1,005–1,162 ms, 86–87 MiB | **86–88 MiB**, 4 | 0–2 ms |
| coreutils manual | 694 | 417–441 ms, 119–120 MiB | **104–107 ms, 78–80 MiB** | 437–465 ms, 96–99 MiB | 99–101 MiB, 4 | 1–2 ms |
| 100,000-row table | 1 | 4,458–4,515 ms, 656–663 MiB | 4,355–4,950 ms, 676–679 MiB | — | 676–679 MiB, 1 | — |
| HN thread | 1 | 1,250–1,302 ms, 211–215 MiB | 1,203–1,251 ms, 211–215 MiB | — | 211–215 MiB, 1 | — |
| Commons gallery | 3 | 18–20 ms, 23–25 MiB | 17–19 ms, 23–26 MiB | 4 ms | 23–26 MiB, 3 | 0–6 ms |
| endless feed | 2 | 1–2 ms, 17–19 MiB | 1 ms, 17–19 MiB | 0 ms | 17–19 MiB, 2 | 0 ms |

- **Worker memory now follows the window plus the parsed document,** not the layout: the
  layout's share is gone (War and Peace 201 → 87 MiB, the standard 642 → 471 MiB, at most
  4 bands and 7,772 leaves resident). What remains is the DOM and the cascade of the whole
  document — 409 MiB of the standard's 471 — which phase 2 does not touch.
- **A document that is one table is one band** (the table; HN, whose comments are one
  table cell): checkpoints are between blocks in plain flow, and a table is laid out
  whole. It costs what the whole layout costs (after a fix, 8bb1f10b: it first cost twice,
  laid out once to find its range and again to keep it). *Since fixed: checkpoints between
  table rows, phase 3 below.*
- *Time to reach a jump target.* Once the first pass has passed it, a jump is one band
  laid out from its checkpoint: 0–6 ms. Before that, the jump waits for the first pass to
  get there; to the end that is the whole layout's time again (the pass costs what the
  whole layout costs: the standard 2.1–2.4 s vs 2.0–2.3 s, with the same text work, 5.01 M
  vs 5.00 M characters, after a fix in 70433d5f — a band whose checkpoint is a child of a
  box it continues walked every sibling before it, 6.6× the text work).
- *A scroll step* (a `want` a screen further, including any band laid out): p50 under
  1 ms, p99 1 ms; the worst is 50–74 ms on the standard and coreutils, a single band
  holding a large `pre` or table, on the worker, not the compositor.

*The scroll harness with bands* (`scroll_bench --source`: the content source on its own
clock, as the worker is its own process, laying out the rest in the background in the
time each period leaves; bands at one viewport; strip compositing; 8 s; one run per cell,
both arms: `source` and the whole scene `scene`):
- **Frame gates pass in both arms, and the arms match**: the compositor's work does not
  depend on where the bands came from. The HTML standard, measured alone on the machine:
  60 Hz p99.9 9.7–16.0 ms, missed vsyncs 0.0%, 0 stalls in every pattern; 120 Hz p99.9
  6.0–9.5 ms, missed 0.0–1.2%. The other documents: 60 Hz p99.9 12–20 ms, missed ≤ 3.1%
  (page-down, which exposes a whole window per step, as before); 120 Hz missed up to 4.4%
  in both arms (the harness compositor's frame cost, §10.5 of pergola.md). One cell is
  left out as contaminated: the table's `scene` arm at 60 Hz (p99.9 63 ms, 10% missed)
  ran while other measurements ran on the same machine; its `source` arm was clean.
- **Placeholders**: none in flings, page-down or Pergola's physics on any document — the
  source keeps a viewport ahead.
- **The content gate fails for one case: a jump to the end in the first seconds**, before
  the first pass has reached it. Real content replaces placeholders after 1.2–1.8 s on the
  standard, 0.6 s on War and Peace, 75–100 ms on coreutils. After the first pass the same
  jump costs one band. Ways to meet it, not built: lay out from the end first when the view
  jumps there (needs an estimate of where the end's checkpoint is, which the first pass is
  what provides), or a first pass that measures heights without painting.

*Counted for the scene-groups review* (fresco-scene-groups.md §6, on the whole layouts):
- **Leaves painted outside their own band** (the overlay's sources): 8,801 in 40 of the 129
  corpus documents, none in the big documents. In the bands checked, 1,919 went into
  another band's group in place (painted under something there) and 5,204 into the overlay
  group (over everything there). Most are a few documents' positioned headers and cards.
- **Opacity groups whose leaves overlap** (multiplied alpha would be wrong; `ISOLATE`):
  481 of 687 groups, in 19 of 30 documents that have any (doc22 alone: 394). None in the
  big documents.
- **Rotated or skewed text and images**: none, on any document.
- **Scene groups per band** (one per run of leaves with the same clip, transform and
  opacity group): p50 8, p90 85, the most 2,237 (doc14); over the 4,096 limit only in the
  HN thread's single band (4,764) — a one-band document, which table-row checkpoints fix.

*Not yet:* ~~the worker's wire protocol for `want`~~ (built, phase 4 below);
~~table-row checkpoints~~ (built, phase 3 below); the image working set (part 3);
checkpoints that survive a restyle (a navigated document is laid out again from the
start).

**Step 3, phase 3 built: checkpoints between table rows (2026-11-06).** A document that is
one big table was one band (above). Now:
- *A band may stop before a row of a table in plain flow* when no cell above spans past that
  row. The next band starts the table at that row, at the y recorded for it, and measures and
  lays out only its own rows, group by group (the rows between two such boundaries, whose
  column slots need nothing from above). The columns are declared (§3.13), so a row's height
  is its own cells'. A table's rows and its boundaries are computed once and kept with the
  band state; a band never walks the 100,000 rows.
- *A row that is one cell holding a long flow* (HN: the page is a table, and its third row is
  one cell holding the story and the table of comments) is laid out as plain flow at its own
  height, so a band may stop inside it, and inside the tables it holds. Only where that is
  exactly what the whole layout does (which gives the cell its row's height as a definite
  height): the row has one cell and no span crosses it; the cell neither clips nor positions;
  nothing in it has a percentage height, offset or `min`/`max-height`; and the first column
  is not pinned (a wide table that scrolls on its own and has two or more columns).
- *Checked.* `tests/checkpoints.rs`: a 400-row table with row spans, paragraphs before and
  after, at bands of 150, 333 and 600 px; no band starts inside a row span; HN's shape (a
  page in one cell of a table, 300 comments as rows of one cell) at 200 and 600 px. The
  oracle, re-run: 129 corpus documents, 3,448 bands (3,234 before: their tables are cut
  now), 1,427 checked, **0 differ**; the 7 big documents, 0 differ — the table 40 of 4,546
  bands checked, HN 40 of 468. Whole-layout NSG byte-identical on all 136.
- *Measured* (`band_peak`, 3 runs per arm, as above; this build, so every row is new):

  | document | bands | whole layout: ms, peak | first screen: ms, peak | first pass to the end: ms | scrolled end to end: peak | a jump from a checkpoint |
  |---|---|---|---|---|---|---|
  | 100,000-row table | 1 → **4,546** | 3,370–4,190 ms, 669–672 MiB | **331–350 ms, 335 MiB** | 4,677–4,747 ms | **414–415 MiB** | 1–4 ms |
  | HN thread | 1 → **468** | 926–952 ms, 211–213 MiB | **182–189 ms, 170–174 MiB** | 595–650 ms | **178–182 MiB** | 3–5 ms |
  | HTML Living Standard | 3,999 | 1,713–1,869 ms, 626–638 MiB | 444–464 ms, 409 MiB | 1,645–1,820 ms | 466–468 MiB | 0–2 ms |
  | coreutils manual | 839 | 328–346 ms, 117–119 MiB | 81–84 ms, 78 MiB | 294–307 ms | 87 MiB | 0–1 ms |
  | War and Peace | 1,998 | 732–769 ms, 202–204 MiB | 24–26 ms, 51 MiB | 815–823 ms | 85–86 MiB | 0–1 ms |

  - The table's first screen falls from the whole layout's 3.4–4.2 s to a third of a second,
    and its worker peak by 38%; what remains is the DOM and cascade of 500,000 elements. Its first pass to the end
    costs about 20% more than the whole layout (each band re-enters the table and sizes its
    columns again). HN's first screen is 5× faster.
  - The HTML standard and coreutils gain bands too (their tables are cut): 3,723 → 3,999 and
    694 → 839, and the most leaves resident fall 7,772 → 3,663 and 28,854 → 1,353.
- *The scroll harness* (both arms, 8 s, this time alone on the machine): **every frame gate
  passes** on the five big documents at 60 and 120 Hz, in both arms, and the arms match:
  p99.9 at most 12.8 ms at 60 Hz (one cell 24.9 ms, coreutils page-down with the source;
  still under 2 periods) and at most 9.4 ms at 120 Hz; missed vsyncs at most 1.5%; 0 stalls.
  No placeholders in flings, page-down or Pergola's physics. **The content gate still fails
  only for a jump to the end in the first seconds:** the table waits 3.7–4.1 s, the standard
  1.2 s, War and Peace 0.27 s; HN 33–50 ms and coreutils none pass it. Laying out from the
  end first, or a first pass that sizes rows without painting them, is what would meet it.

**Step 3, phase 4 built: bands over the wire, and far jumps without the layout above
(2026-11-06).**
- *The protocol* (pergola.md §10.4, now the broker's). `Request::Want { session, range,
  zoom_64, prefetch }` and `Request::BandsIdle { session, bands }` are answered by
  `Event::Bands`: the source's events (`extent`, `band_ready`, `band_updated`,
  `band_evicted`), each band's group clipped to its range and the overlay as NSG, and the
  groups cleared. The worker holds the session's source (`display::Bands`, wire `BND`),
  built from the normalized document — normalized once per recording and stored by the
  broker, as for `Scene` — and built again when the broker's per-session document
  generation changes (navigate, back, forward, restore, swap). The stored document is sent
  only to a worker building its source.
- *Checked by the broker* (`check_bands`), whatever host answers: at most 4,096 events, 64
  groups and 20 MiB of NSG an answer, each group NSG and at most 8 MiB; a band's group only
  for a band the same answer announced, clipped to exactly its announced range; no clip on
  the overlay; the normalized document as for a scene; asks bounded before a worker sees
  them (coordinates within ±2⁴⁰, zoom 25–500%, at most 64 bands of idle work). A malformed
  jailed answer retires the worker; the wire form's parser is strict.
- *Tested* (`navigator-backend/tests/bands.rs`): both hosts through the broker (first screen,
  a second session from the store with identical groups, idle to settled, a jump evicting the
  top, a navigated document's source rebuilt, refusals); the broker's refusals and the wire
  round trip; and **Pergola's own scroll physics and `Viewport`** (pergola/src/scroll.rs,
  compiled in) **driving a jailed worker** through swipes and flings down the document, with
  no frame showing a placeholder and at most 5 bands held.
- *Far jumps.* When a `want` is far beyond the first pass, the source lays out two more bands
  of it and, if that does not reach, a **provisional band** at once (`Banded::provisional`):
  from the block at the fraction of the document the target is of the estimated extent
  (`Prepared::jump_target`: the first block at or after that node, climbed to the box just
  inside its flow container), placed at the target y, with nothing above it laid out. Its
  text and boxes are the document's; its y is an estimate, and a box it starts inside of
  paints nothing. It has an id of its own (`source::PROVISIONAL | n`); the first pass goes on
  in idle time, and when it reaches the range the exact bands are made ready and the
  provisional one evicted in the same answer (the view anchors the change, §10.4). It never
  touches the exact bands' state: the oracle and byte identity are unchanged.
- *Re-checked.* Oracle: 129 corpus documents, 1,427 bands, 0 differ; the 7 big documents,
  0 differ. Whole-layout NSG byte-identical on all 136. `tests/source.rs`: a jump three
  quarters down a 400-section page gets a provisional band covering the rows, without laying
  out what is above, holding paragraphs from about three quarters through, replaced by exact
  bands once idle work arrives.
- *Measured* (`band_peak`, 3 runs per arm; "jump" is from the first screen straight to the
  estimated end):

  | document | first screen | the jump: content shown | exact end (first pass, in idle time) | peak |
  |---|---|---|---|---|
  | 100,000-row table | 340–344 ms | **3.3–3.5 ms** | 4,463–4,494 ms | 414–416 MiB |
  | HTML Living Standard | 445–453 ms | **3.9 ms** | 1,661–1,742 ms | 471 MiB |
  | HN thread | 184–191 ms | **3.7–4.0 ms** | 597–617 ms | 177–178 MiB |
  | War and Peace | 24 ms | **0.8 ms** | 793–804 ms | 85–88 MiB |
  | coreutils manual | 80–82 ms | **1.3 ms** | 296–305 ms | 87–88 MiB |

- *The scroll harness* (both arms, alone on the machine; the harness now times a gap from
  when it appears, and applies an eviction when its answer arrives, with what replaced it):
  **every 60 Hz gate passes, the early jump included**, and at 120 Hz every one but
  page-down's missed vsyncs on HN and coreutils (2.8–3.2%, the same in both arms: the
  harness compositor's cost of a page-down's strips, as before). Frame p99.9 at most 14.2 ms
  at 60 Hz and 11.3 ms at 120 Hz, 0 stalls.
  A jump to the end one second after display shows real content within **17 ms** of the
  scroll settling on all five big documents (the gate is 100 ms; it was 0.27–4.1 s); the
  exact bands follow 0.18–0.25 s later on HN, 0.7–0.8 s on War and Peace, 2.2–2.3 s on the
  standard, 3.0 s on the table, as the first pass reaches the end.

*Gates for bands (proposed):* at 60 Hz, 0 stalls over 100 ms after first display in every
pattern, p99.9 frame time ≤ 2 periods, and missed vsyncs ≤ 1%. At 120 Hz, the same once
the compositor redraws by damage.

**Scrolling belongs to Pergola (agreed with the user, 2026-11-06).** Flings, momentum, the
shift-then-draw-the-strip compositor, fast-fling motion blur, placeholders for regions not
yet laid out, and reduce motion are specified in pergola.md §10.1 as `ScrollView`
behaviour, so every app scrolls the same way. This section owns the content side only:
bands, checkpoints and the resident window, which feed `ScrollView` through the band
interface (pergola.md §10.3). The harness above stays the judge of both; it measures
strip redraw and placeholders, but they are built in Pergola and frescod, not here. The
same layout is offered to other apps as Limen's `doc-viewer` (untrusted content) and a
Pergola `DocumentView` (trusted content), pergola.md §10.2.

**Decisions to settle:**
1. Raise the profile ceilings once bands and the image working set are in, or split very
   large documents into chapters at conversion? *Recommend raising them,* to levels
   measured on the set above, keeping a hard cap; the resident window becomes the real
   bound.
2. Lay out the rest of the document in the background after the first screen, with a
   scroll extent that settles once? *Recommend yes;* the alternative is waiting for the
   whole layout before showing anything.
3. Virtualise very long tables and feeds in the document lane? *Recommend yes,* only above
   a measured size, so small pages keep the simple path.
4. The resident budget per document. *Recommend starting at 64 MiB of scene* plus the
   image working set, and two screens of prefetch either side, then tuning by the frame-time
   measurements.

### 4.9 Zoom and text size (approved by the user, 2026-11-06)

*Asked by the user, 2026-11-05: zoom and bigger fonts.* Four ways a reader changes size.
None of them re-converts or re-normalizes: a normalized document carries every media
context, so each is a fresh LAYOUT of the same document.

1. **Page zoom.**
   - The document is laid out at the window's width divided by the zoom (CSS px), so
     media queries are evaluated at that width, as a browser does.
   - It is painted at device scale × zoom. The broker's `device_scale` already scales
     every image request, so an image is re-decoded at its new painted size within the
     decode budget, and an SVG image is drawn again at it. Vector paths move by
     `ATRIUM_CORE_FILL_TRANSFORM`, with no new geometry.
   - The reader stays on the same element: the anchor of §4.8 and progressive
     display, an element and an offset into it, is mapped through the new layout's
     geometry.
2. **Text size only.**
   - A user text scale multiplies font sizes. The profile's fixed 16 px root becomes
     16 px × scale, and `px` font sizes scale too. `em`, `rem` and `%` follow from them,
     and the page reflows.
   - An inherited font size is the parent's computed size, already scaled, and is
     not scaled again. A root sized relatively (`html { font-size: 100% }`) resolves
     against the reader's initial 16 px × scale. Margins, padding, borders, viewport
     units and media queries do not scale.
   - *Tables.* Each column's min-content and max-content widths (profile §3.13) are
     measured again, exactly, in the worker, at the reader's text size. The
     normalizer's pre-measure sheet is marked `atrium-premeasured`; at 100% it is used
     as is, and at any other size it is dropped and the columns are measured at that
     size. The normalized document, and its cache entry, do not depend on text size.
   - ★★ *Not scaled.* The first design scaled the 100% widths by the text scale
     (rounded to the nearest 1/64 px, halves away from zero). The corpus refuted it
     (2026-11-06; 1,705 columns on 36 of the 122 pages):

     | text size | within 1 px of exact | more than 1 px too narrow |
     |---|---|---|
     | 125% | 79% | 0.4% |
     | 200% | 31% | 3.5% |
     | 300% | 29% | 4.8% |

     A column holds parts that do not scale: padding, borders, images and an
     author's px floor. Most scaled columns came out too wide, and a too-narrow one
     clips its text.
3. **Pinch zoom.**
   - The compositor magnifies the scene it already has, immediately: fills by
     transform, the rest as composed.
   - When the gesture settles, the page is laid out and drawn again at the new zoom,
     sharp.
4. **Wide tables.** A table whose min-content width exceeds the available width becomes
   its own horizontally scrolling region, so the rest of the page still fits the window.
   - NSG had no scroll offset (`auto` and `scroll` clip like `hidden`), so this needs a
     `scroll` node. It is validated by the broker like every other node, and scrolled
     by the UI and compositor, never the worker.
   - Optionally, the table's first column stays in view.

*Tests.* Every zoom level from 50% to 300% is byte-reproducible; the anchor is kept; no
image is drawn below its painted resolution after a zoom settles; a zoom step is timed
on the corpus and on the §4.8 big documents; tables that overflow are counted at 100%,
200% and 300% before and after; and pages whose tables fit are byte-identical.

*Built (M1: items 1 and 2, and item 1's re-decode).*
- `Env::zoomed` lays a page out for zoom. `Env.text_scale_64` scales text in the
  cascade.
- `remeasure_tables` re-measures table columns at a text size other than 100%.
- The broker's `set_zoom` is per session, from 25% to 500%. Image requests are
  decoded at painted size × device scale × zoom, and each re-decode replaces that
  image's charge rather than adding to it.
- `navigator_render::anchor` finds an anchor in the geometry and maps it through the
  new layout.

*Measured (2026-11-06, `navigator-prerender/examples/zoom_measure`).* The corpus is the
122 readable documents, each at 8 levels from 50% to 300%, by page zoom and by text
size: 1,952 layouts.
- **Reproducible.** Two renders at every level gave identical NSG; 0 of 1,952
  differed. All 56 big-document layouts also reproduced.
- **Byte-identical at 100%.** Every layout at 100% equals the plain render. On the
  421-document identity set, the scenes are identical to the build before zoom. The
  normalized documents that have tables (115) gain the `atrium-premeasured`
  attribute and nothing else.
- **Anchor.** There are 1,098 reading positions, 9 per page at 10% to 90% of its
  scroll range. None was lost at any level. Two cases leave a position elsewhere,
  and both have a defined answer:
  - *Pinned to the end.* When the page got shorter than the position (zoom out,
    and smaller text), it was pinned to the end: page zoom 166 at 50%, text size 7
    at 300%.
  - *Element hidden.* A media query at the new width hid the element: page zoom
    16 to 36. The caller keeps the same fraction of the document.
- **Image resolution.** After a zoom, every image comes back at painted size ×
  device scale × zoom, from 50% to 300% (`navigator-backend/tests/image.rs`).
- **The zoom step is one full layout and scene** of the cached normalized document.
  Text size adds the column re-measure.

  | | p50 | p90 | max |
  |---|---|---|---|
  | page zoom, corpus | 21–23 ms | 139–183 ms | 492 ms |
  | text size, corpus | 22–23 ms | 163–193 ms | 744 ms |
  | page zoom, big documents | | | whatwg 2.5 s, 100k-row table 4.7 s |
  | text size, big documents | | | whatwg 4.3 s, 100k-row table 8.1 s |

  On a big document a zoom costs what its first layout cost. That is the case for
  §4.8's bands and checkpoints. Until then, pinch (item 3) is what keeps the zoom
  immediate.
- **Tables needing more than the available width** (page zoom, min-content against
  800/zoom px), before item 4: 9 tables on 6 pages at 100%, 72 on 17 at 200%, and
  220 on 24 at 300%.

*Built (M2: item 4, wide tables).*
- A table box wider than its containing block becomes a scroll region
  (`navigator_render::scroll`). Its layout does not change: the box, its border and
  background, and every cell keep their positions.
- They are painted under a clip of the containing block (the region's viewport) and
  under a translation transform that the UI owns. NSG declares it as:

  ```text
  scroll c<viewport> x<i>[,<j>…] <content x> <y> <w> <h>
  ```

  The `x` list holds the region's own translation and every transform nested in it.
  The content box bounds the offset. A right-to-left table, which overflows on the
  left, scrolls the other way.
- The worker lays the table out once and never learns the offset. The compositor
  moves every node under those transforms, along with the own clip of any vector
  path among them. `scroll::offset_scene` is the model of that, and it clamps the
  offset to the content box.
- *The broker's check* (`scroll::validate`, the first scene-graph rule of §4.2 in
  code) refuses, never clamps:
  - too many regions;
  - an index that names nothing;
  - a region transform that is not a pure translation of its parent;
  - a transform claimed by two regions;
  - a negative content size;
  - ★ any node a region moves that is not clipped to that region's viewport. Moved,
    it would paint over the rest of the page.
- *Clips inside a region (2026-11-06).* A table that clips itself, or holds a clip,
  scrolls too.
  - A clip made inside a region is intersected with the clip outside the region, not
    with its viewport. It is listed in the `scroll` line as `k<i>,…`, moves with the
    content, and is intersected with the viewport when painted
    (`scroll::effective_clip`).
  - Before this, such tables stayed overflowing: 18 of 169 at 300%. The broker
    refuses an inner clip that is the viewport or another region's, and any node
    clipped by an inner clip that the region does not move.
  - The first corpus run with plain regions found the case the hard way: hi and ru
    Wikipedia's tables clip themselves, and the broker refused those regions.
- *The first column stays in view (2026-11-06).*
  - In a region, the first column's cells keep the transform outside the region, so
    the compositor does not move them. They are clipped to the viewport and painted
    after the other columns, over the background behind them, so the rest slides
    under them.
  - A first column wider than 40% of the region is not pinned.
  - A pinned cell inside a self-clipping table is clipped to the viewport, not to the
    table's own clip, which moves. ru Wikipedia showed this; the broker refused it
    otherwise.
- A region does not nest: a table inside one is moved by it.
- *Not built:* pinch (item 3), whose settle is a page-zoom layout.
- *Measured with both (2026-11-06, the same 1,952 corpus layouts).* No wide table
  is left overflowing at any level: 47 regions at 100%, 157 at 300% page zoom and 269
  at 300% text size. The broker's check passes on every layout.
  - On the identity set, the documents that changed are those with regions, compiled
    captions, or images with percentage limits. The rest are byte-identical, and two
    runs gave identical output.

*Measured (2026-11-06, the same 1,952 corpus layouts and 56 big-document ones).*
- Before this change, every table here overflowed its containing block. After it,
  only the tables refused for a clip still do; the rest scroll on their own:

  | | overflowed before | still overflowing after | scroll on their own |
  |---|---|---|---|
  | page zoom 100% | 55 tables, 12 pages | 4 tables, 3 pages | 51 |
  | page zoom 200% | 64 tables, 11 pages | 9 tables, 5 pages | 55 |
  | page zoom 300% | 169 tables, 21 pages | 18 tables, 8 pages | 151 |
  | text size 200% | 133 tables, 24 pages | 9 tables, 7 pages | 124 |
  | text size 300% | 286 tables, 30 pages | 15 tables, 9 pages | 271 |

- The broker's check passes on every layout: 0 refusals in 2,008.
- Every layout is still reproducible, 100% is still identical to the plain render,
  and no anchor is lost.
- *Pages whose tables fit are byte-identical.* On the 421-document identity set
  against the build before item 4 (8760ab3d), 373 NSG outputs are identical. All 37
  that changed gained regions (195 in all).
  - Stripped of `clip`, `xform` and `scroll` lines and of `c`/`x` attributes, every
    node in those 37 is identical, so no layout moved.
  - Two full runs gave identical digests.
- Conformance row 64 (vertical-align) is five 150 px cells, 2 px wider than their
  784 px body. Its golden gained a region and the cells' `c0 x0`, and no position
  changed. 64/64 still match.
- The zoom step is unchanged: corpus p50 21.6–23.3 ms, p90 140–154 ms.

## 5. The store is Tessera

`atrium-doc://<hash>` *is* a content address in the root filesystem's CAS. This is what
Tessera-as-default-root was groundwork for; it is not a cache the navigator has to write.

- **"Do we have it" is an existence query**, not a cache lookup with its own expiry
  policy — and therefore not a second implementation of freshness semantics to get wrong.
- **Integrity is the storage layer's job**, already verified on read.
- **Dedup** across documents and across the system: a font or library shipped by many
  origins is stored once; an app bundle sharing blobs with something installed is nearly
  free to fetch.
- **Offline** falls out of content addressing: what you have seen stays addressable.

### 5.1 Dedup domains — assignment, not new mechanism

Cross-domain CAS dedup is an existence oracle, and Atrium already settled the mechanism
(tessera-fs.md §20: `global` / `deferred` / `salted`, boundary = quota domain;
possession-scoped negotiation in aqueduct.md §6.6). The navigator only has to assign
domains correctly:

| content | domain | why |
|---|---|---|
| app bundles (signed, via Opifex) | `global` | trusted ingest — where the N-apps-≈-1×-disk win actually lives |
| web-fetched document subresources | `deferred`, **partitioned per top-level site** | untrusted ingest; matches the cache-partitioning the web itself adopted to kill cross-site probing |
| per-user overlays, session data | `salted` | secrets must not dedup across domains |

Two asymmetries worth stating because they are easy to get backwards:

- **The JS-free invariant removes the usual probing vehicle on the document lane.** The
  classic primitive — script times a subresource load and infers a prior visit — needs a
  scripted timer and a scripted fetch. Documents have neither. This is a real reduction
  in attack surface, but it is *not* a licence to dedup documents globally, because of
  the next point.
- **Possession is still observable to the origin server.** If the client skips a fetch
  for content it already holds, the server learns the client held it — an oracle that
  needs no script at all. So a cache hit must only benefit a site that served that
  content: the possession-ledger discipline of aqueduct.md §6.6, applied to HTTP. Apps,
  which *do* run code, get the partitioning discipline in full.

---

## 6. The Atrium Document Profile — the decision that makes this finishable

An HTML/CSS engine is unbounded work. Without a written line, every page anyone tries
becomes a bug report against an infinite backlog, and the component quietly becomes
Servo — which atrium-navigator.md §6 already assigns elsewhere (last, and server-side).

**Normative: the document lane targets a versioned subset, specified in its own
document, not "the web".** Profile v1:

- block and inline layout, flexbox; **no** floats-era quirks mode, **no** scripting;
- a named, shipped font stack — web fonts are a later, separately-argued addition;
- an enumerated CSS property set: anything unlisted is ignored, and *reported* as
  ignored rather than silently dropped;
- Markdown and a bounded HTML subset as input grammars.

Conformance against this profile is the acceptance criterion. Arbitrary legacy pages are
explicitly out of scope for this component.

**Build order inside the profile: Markdown first.** It is small enough to prove the
whole pipeline end-to-end — resolve, fetch, jail, parse, layout, serialize, validate —
in days rather than months, which puts every seam in this document under test before any
of the hard layout work starts. Then the HTML subset, then CSS layout.

**PDF is split out entirely**: separate parser, separate threat surface, separate
schedule. It is not part of Profile v1.

---

## 7. Test strategy

Everything here runs headless, in CI, with no display server.

- **Golden scene-graph tests.** Fixture → NSG → compare. Gated on §4.1 determinism.
- **Profile conformance.** A curated corpus, reported as a **number**, not pass/fail: a
  checker with no coverage figure passes vacuously, and "all green" against three
  fixtures has told us nothing here before.
- **Parser fuzzing.** Reuse the two-phase `scripts/core-fuzz.sh` shape for the HTML, CSS
  and NSG-validator parsers. Report **coverage actually reached**, never exec count — a
  fuzzer that runs is not a fuzzer that tests.
- **Capability assertions, tested with the syscall.** A worker fixture that *attempts*
  filesystem and network access, asserting refusal at the syscall rather than trusting
  the jail config to mean what it says. Every capability cell in §2's table is one test.
- **Resource bounds.** Per-jail RCTL limits on the worker; assert a hostile fixture
  (deep nesting, huge tables, decompression bombs) is refused within bounds rather than
  taking the broker with it.
- **Control-plane driver.** The full navigation state machine — history, back/forward,
  session restore — driven by a script against `navigatord`, with no UI in the process
  tree.

---

## 8. Milestones, each with a gate that can fail

| | milestone | gate |
|---|---|---|
| **M0** | Markdown → NSG, hermetic | byte-identical output, 3 runs × 2 machines |
| **M1** | jailed fetcher | fetcher's filesystem access refused **at the syscall**, asserted by test |
| **M2** | Document Profile v1 (HTML + CSS) | curated corpus with a reported conformance **number** — rows **64/64**, corpus **29/29 after the normalizer** (§8.4 measured 1/29 raw, §8.5 closed it). Converter must still carry stylesheets and image sizes |
| **M3** | per-document jail + graph validator | worker capabilities == none; validator fuzzed with reached-coverage reported; **per-document jail launch cost measured**, not assumed — **PASSED** (§8.6) |
| **M4** | navigation state machine + history in Tessera | back/forward/session-restore driven headlessly — done in-process and through the jailed host, in a real jail (§4.4, §8.7, §8.8); a store on a Tessera volume is open |
| **M5** | UI attach (Pergola chrome + Limen document surface) | **UI deleted ⇒ suite still green** |
| **M6** | app-launch path (`atrium-app://` → Nomenclator → Opifex → jail) | trial-launch consent gesture; signature verified before launch |

---

### 8.1 M1 — PASSED (2026-09-22): `navigator-fetch`

The fetcher can reach the network and cannot name a file. That is enforced by the
kernel, not by the code's good behaviour.

**Mechanism, FreeBSD-native: Capsicum + casper.** At startup, and only then, the fetcher
does everything that needs a global name:
1. reads the system trust store (`/etc/ssl/cert.pem`) into `rustls`;
2. forks casper's `system.net` service, limited to resolving names on **ports 80 and 443
   only**, and to connecting only to addresses **it resolved** (`CAPNET_CONNECTDNS`);
3. calls `cap_enter()`. It refuses to serve unless `cap_getmode()` confirms the mode.

After that, a request is: resolve and connect via casper, TLS (`rustls` + `ring`), then one
bounded HTTP/1.1 GET. The GET sends no cookies, Referer or credentials; asks for
`identity` encoding and refuses a compressed body; allows 64 KiB of headers and 8 MiB of
body; times out after 20 s; and follows at most 5 redirects, never https→http.

**Gate, run in the VM as uid 1001** (`navigator-fetchd --gate`):

| probe after `cap_enter` | result |
|---|---|
| `open` of the trust store, `/etc/passwd`, `/`, `/tmp/…`; `open(O_CREAT)`; `stat(/etc)`; `openat(AT_FDCWD, …)` | all **ECAPMODE** (errno 94): refused before any lookup, not ENOENT or EACCES |
| direct `connect()` | **ECAPMODE** |
| casper connect to an address it did not resolve | refused (ENOTCAPABLE) |
| casper resolve on port 8443 | refused by casper's own limit, not only by ours |
| https fetch through casper + TLS | 200 |

**Controls:**
- the same trust store is readable *before* `cap_enter`;
- a `--gate-control` arm that skips `cap_enter` opens the same files, so every gate row
  would fail without the mode (the check reads the mode, not a constant);
- serve mode: https and http 200, `file://` refused, an unresolvable host fails cleanly.

**And inside a jail (same day).** The fetcher ships as a signed bundle,
`org.atrium.navigator.fetcher`: `network = "full"`, its own copy of the trust store, and
its library closure (libcasper, libcap_net, libnv…) resolved by `opifex`. Launched through
the one-shot lane as uid 1001, the whole gate passes **from inside the jail**. The run:
- the jail was `app-org-atrium-navigator-fetcher--f1`, with its own `vnet`;
- three `navtest` processes: the fetcher and casper's two;
- the fetch went out through the routed epair and the per-app pf anchor;
- afterwards there were no jails, epairs or anchors.

So the layers compose: the jail decides what network exists at all, and capability mode
decides that nothing in the process can name a file or an address.

**Gap found: a headless component cannot be granted through the daemon lane.** The first
attempt used `portcullis exec --daemon`, which was correctly refused: a networked app
needs a grant and "there is nobody to prompt on this path". But `navtest` cannot save a
grant either, because `/var/db/atrium/<user>/` is root-owned. That matches §7: a user
process that could write its own policy could grant itself capabilities. The mechanism
§7 names for this case, the trusted-installer policy (`/etc/atrium/policy.toml`
pre-grants), was not built. The in-jail run therefore used the direct lane (root caller,
`--user navtest`). That lane consults no policy at all, so an earlier "root's own grant"
here was wrong. **Since built** (portcullis.md §7.1): with
`portcullis policy grant --system org.atrium.navigator.fetcher`, the daemon lane launches
the fetcher unattended as uid 1001, and the gate passes inside the jail.

**The converter fetches through it (same day).** `PRERENDER_FETCHD="<cmd…>"` routes
every fetch of a conversion through one `navigator-fetchd`: script loading and the page's
own same-origin network, which share one process. Typically the command is
`portcullis exec --daemon --instance conv-{instance} org.atrium.navigator.fetcher`, so each
conversion gets its own jailed fetcher, launched unattended under the system grant
(portcullis.md §7.1). The client (`FetchdFetcher`) treats the reply as untrusted input:
- the header line is bounded, and a stated body over 8 MiB is refused unread;
- a lying length marks the fetcher broken rather than desyncing the stream;
- a URL with a line break is refused **before sending**, since the protocol is one
  request per line and the page would otherwise choose a second request;
- non-2xx is a failure, as with the instrument's `curl -f`;
- shutdown is bounded: end-of-input, a 5 s grace for the launcher's jail teardown,
  then kill.

**Measured in the VM, all 104 documents of corpus 1, as uid 1001:**

| arm | external scripts |
|---|---|
| direct `curl` (control) | referenced 469, fetched **467**, failed 2 |
| through `navigator-fetchd` | referenced 528, fetched **526**, failed 2 |

The fetcher arm used 103 unattended launches under the system grant, with at most 3
fetcher jails at once. Referenced counts differ because module import graphs are
discovered live, so each arm walks what the network served at that minute.

**Getting that run clean found three bugs:**
1. **Harness, and the lesson again.** The first full run had `/tmp` (a 20 MB tmpfs) fill
   up mid-copy, taking `manifest.tsv` with it, so every number was wrong. And an ad-hoc
   harness checked jails, epairs and anchors but **not mounts**, which the committed E2E
   checks; the leak below had been present in earlier runs that reported clean.
2. **A daemon stopped mid-teardown leaks its one-shot instances, forever.** Teardown
   removes the jail, then unmounts the instance root. The harness stopped portcullisd as
   soon as the converter returned, and six instances were caught in between: roots with
   nullfs + tmpfs + unionfs mounts, three with process-less jails and routed nets. Because
   instance tags are unique (a pid), the reclaim-on-reuse path never met them. **Fix:**
   `portcullis_oneshot::reclaim_abandoned`, run at portcullisd start (after the bind),
   tears down `app-…--…` instance roots whose jail is gone or empty and which are older
   than 60 s. The age guard is for the direct lane, which builds roots outside the
   daemon.
3. **jaild kept records of vanished jails.** Its startup reconcile freed a vanished routed
   jail's `/30` but kept the jail record, so the state file grew by one record per such
   jail. **Fix:** the same reconcile drops records whose name no longer resolves to the
   same jid.

**Verified:**
- stopping the daemon at once reproduces the leak (1 root, 3 mounts); after 60 s, a
  restarted portcullisd reports "reclaimed abandoned one-shot instance" and the machine
  is at zero;
- the redeployed jaild pruned the three real stale records;
- **control:** the same batch with the daemon stopped 15 s later leaks nothing, so the
  leak is the interrupted teardown, not the conversions;
- the corpus E2E is unchanged (98 documents, no leaks).

**Not yet:**
- a per-host request count for the report (legacy-web §5.4.1d, rule 4).

Cross-building `ring` needs a C compiler for the target: Homebrew clang with
`--target=aarch64-unknown-freebsd --sysroot=sysroot` (see `navigator-fetch/Cargo.toml`).

### 8.2 M2 — plan (2026-09-22)

**The renderer's input is a profile-conformant document, not a web page.** The profile
(atrium-document-profile-v1.md §6) puts tolerance in a separate, jailed **normalizer**;
the renderer is strict, and anything outside the profile is a diagnostic with a source
position (§5.1), never a silent recovery. So M2 is two things: a strict renderer, and
**a curated conformance suite**.

**The number.** The denominator is the profile's **64 property rows** (§3.3–§3.9). A row
counts as *exercised* when fixtures cover all its admitted values, and as *matched* when
those fixtures' NSG equals a reviewed golden. The report prints both counts, never a
green tick.

**Built in layers, each raising the number:**
1. **M2.1 `navigator-style`:**
   - a CSS tokenizer and parser (own code: the profile's grammar is small, and the parser
     is a hostile-input surface whose bounds we want to own; `cssparser` is MPL-2.0,
     case-by-case under the licensing policy);
   - the 64-row value grammar;
   - admitted selectors (no descendant combinator);
   - diagnostics with positions;
   - then cascade (UA layer, author source order, specificity within a layer), inheritance,
     initial values, and computed style.
2. **M2.2:** block and inline layout with the fixed box rules (`border-box`, no margin
   collapsing, no floats), and paint of backgrounds and borders into NSG.
3. **M2.3+:** flex, grid, tables, the remaining paint rows, and the semantic tree (§4).

**Where M2 stands (2026-09-23).** Every one of the profile's 64 property rows is
exercised and matched (§8.3). ★ **That is not all of M2's gate.** The gate reads
"curated corpus with a reported conformance number", and the corpus leg has NOT been run:
the number comes from fixtures written to exercise each row, which is a different claim
from real documents rendering correctly. The converter already produces 98 real documents
(§8.1); putting them through the profile renderer and reporting what refuses is the next
step, and it is the one that will find what the fixtures could not. The semantic tree
(§4) is also still open.

The M0 Markdown path stays; it becomes one more input grammar in front of the same
layout.

### 8.3 M2 progress — the number, first reading (2026-09-22)

**CONFORMANCE: exercised 64/64, matched 64/64 — COMPLETE (2026-09-23)** (13 on first reading; see the updates
below). Printed by `nsg-conformance`; the
matched set is pinned by a test so it cannot fall silently.

- **Built:**
  - `navigator-style`: tokenizer, the 64-row grammar, selectors, sheets, cascade; 34
    tests.
  - `navigator-render::html`: block and inline layout, box paint; 8 tests.
  - The conformance harness.
  - `nsg-raster` (review tooling, feature-gated).
- **Matched (13):**
  - box rows: margin, padding, border-width, border-color;
  - type rows: color, font-family, font-size, font-weight, line-height,
    text-decoration-line, text-decoration-color, text-transform;
  - paint rows: background-color.

  Every golden was **looked at** before it was trusted. One apparent miss (a red 1 px
  underline that read as dark next to dark text) was settled by reading the pixels,
  which were exactly `#cf222e`, not by eye.
- **Exercised, not matched (11), and why:**
  - display (flex/grid/table laid out as block, inline-block skipped);
  - position (absolute/fixed skipped);
  - insets (`%` top/bottom);
  - width/height (`%` height, min/max-content);
  - min-/max-size (`%` heights);
  - border-style (dashed/dotted drawn solid);
  - font-style (no italic face ships);
  - text-align (justify);
  - white-space (pre-wrap);
  - background-image (not painted).
- **Not yet exercised (40):** everything the layout does not read (flex, grid, tables,
  radius, overflow, spacing, lists, shadow, transform, …).

**Honesty machinery, each part tested:**
1. A row counts as exercised only when its fixtures declare **every** admitted value,
   checked against the grammar table. The control drops one keyword and the row falls
   out.
2. The layout lists the rows it reads (`READ_ROWS`). A non-initial value in any other
   row is counted as unimplemented on every element that has it, so an ignored property
   cannot be silently dropped. Control: `opacity: 0.5` is counted, and `opacity: 1` is
   not.
3. Value-level gaps inside a read row are counted where they occur, including three
   found by re-reading the code after that rule existed: a `%` top/bottom resolved
   against 0, and `%` min/max-height silently ignored.

**Update — matched 13 → 20:**
- **Percentage heights, insets and min/max** resolve against a *definite* containing
  height, which is passed down from the viewport. Against an indefinite one they behave
  as `auto`, which is CSS 2.1 §10.5 and was wrongly counted as a gap before.
- **`min-content`/`max-content`** come from a measuring pass: the widest unbreakable
  word, and the unwrapped line.
- **justify** puts the free space in the collapsible spaces of every line except the
  last and `<br>`-ended ones (integer division, remainder to the first gaps).
- **pre-wrap** preserves spaces and newlines, wraps at space boundaries, and trailing
  spaces hang.
- **dashed and dotted borders:** dashes 3t on and 2t off; dots are round (NSG `rect`
  gained an optional `r<radius>`, written only when non-zero, so the M0 golden is
  unchanged).

**Found in review, not by tests:**
- **`nowrap` wrapped.** It flushed a word at every space, so every space was a break
  opportunity; a test now pins one line.
- **Two fixtures could not show what they certified:** empty boxes for the intrinsic
  widths, and justify on one word, which is the last line and is never justified. Both
  were strengthened before blessing.
- **A stale review binary.** The feature-gated `nsg-raster` was not rebuilt by
  `cargo run`, so a fixed bug still showed in a PNG drawn by old code. The review tool
  is rebuilt before every review.

**Update — lists, matched 20 → 22.** **Decision:** the profile admits `list-style-*` but
not `display: list-item`, so it never says what carries a marker. **`li` carries it**:
- typed by the inherited `list-style-type` (disc •, circle ◦, square ▪, decimal);
- numbered among its `li` siblings from `<ol start>`;
- `outside` hangs left of the first line box, on its baseline, even when that line is
  inside a nested block; `inside` is the first inline item and wraps with the text.

This interpretation should be written into the profile itself (§3.8) when it is next
revised.

**Update — flexbox, matched 22 → 31 (all nine §3.4 rows).** CSS Flexbox §9 without
`order` or `*-reverse`:
- flex base sizes from `flex-basis` (length, `%`, `auto` → the main size or content,
  `content` → max-content);
- lines under `flex-wrap` with gaps;
- grow by `flex-grow` and shrink by `flex-shrink` × base, then clamp;
- `justify-content`, `align-items`/`align-self` (including stretch of auto cross sizes)
  and `align-content`;
- items laid out at a forced border-box size, with heights measured by a scratch layout
  that is rolled back.

Decisions and findings:
- **Baseline alignment** uses each item's first line box. A box without one
  synthesizes it from the bottom of its border box. **`align-content: baseline`** on a
  flex container that is not in a baseline-sharing group falls back to `start` per CSS
  Box Alignment, so laying it out as flex-start is the specified behaviour.
- **Automatic minimum size:** a flex item's `min-width`/`min-height: auto` is
  min(definite specified size, min-content), so items never shrink below their
  content. It was 0, found in review. It is proven by a unit test with a control (an
  empty item in the same place does shrink) and by fixture 21.
- **An inline `style` attribute is a diagnostic** (`input.style-attribute`). The
  profile's cascade has no inline layer, and it had been silently ignored; found when a
  fixture accidentally carried `style=""`.
- **A review misdiagnosis, recorded:** fixture 21's overflowing text looked like a
  shrink bug, but the items were fixed-width by specificity, and the overflow was
  correct CSS. Checked against the NSG's widths before any code was changed for it.
- **Loose text directly in a flex container** is not made into an anonymous item yet;
  it is counted.

**Update — text and paint rows, matched 31 → 38** (letter/word-spacing, text-indent,
overflow-wrap, tab-size, font-variant-numeric, visibility, outline):
- **Spacing** is added after each glyph (and after each space for word-spacing) in
  1/64 px, outside the font-unit pen, so the integer scaling of shaped advances is
  untouched.
- **Tabs** advance to the next stop, a multiple of `tab-size` × the space advance
  measured from the line start — position-based, so it is exact in proportional fonts
  too, not a fixed number of spaces.
- **`overflow-wrap: break-word`** breaks a word wider than a whole line between
  characters, greedily.
- **`visibility: hidden`** keeps the box and its space, paints neither background,
  border nor text, and makes its text non-hit-testable — while a descendant that sets
  `visible` still paints.
- **Outlines** paint outside the border box after the content, sharing the border's
  dash/dot geometry.
- ★ **`font-variant-numeric: tabular-nums` is satisfied, not exercised.** Every shipped
  face already has tabular figures ('1' and '8' have equal advances) and none carries a
  `tnum`/`pnum` feature, so `normal` and `tabular-nums` render identically — which is
  correct. The `tnum` feature is passed to the shaper for web fonts that carry it, and
  that path is **unverified**: no shipped face can exercise it.

Three of the four problems in this batch were **fixtures, not the renderer**: a box too
short to contain its own child, white text on white, and a font set that cannot show the
property. Each was found by looking, and two were settled by reading the NSG rather than
the picture.

**Update — tables, matched 38 → 40** (`border-spacing`, `vertical-align`; `display:
table/-row/-cell`). Separate borders only, as the profile has no `border-collapse`.

★ **Column widths are declared, never measured.** §3.13 requires it and forbids a
fallback path, so the first row's cells give the columns and **a table whose first row
leaves a width `auto` is refused with a diagnostic and not laid out**
(`table.column-width-undeclared`). Tested both ways.

- Rows are as tall as their tallest cell; cell boxes fill the row (separate-borders
  model), and `vertical-align` positions the CONTENT inside that height — including
  `baseline`, where cells of very different font sizes share one baseline.
- **Decision:** the HTML parser inserts `<tbody>`, but the profile's `display` has no
  `table-row-group`, so elements between a table and its rows are flattened.
- The "table part outside a table" counter fires only for a STRAY part in normal flow —
  it first fired for every cell the table itself placed, which would have made the rows
  permanently unmatched.

**Update — border-radius and aspect-ratio, matched 40 → 42.**
- **NSG gained two rect fields**, both written only when non-zero so every existing
  golden stays byte-identical: per-corner `r<tl>,<tr>,<br>,<bl>` and `b<ring>`, a band
  inside the rect's edge. A rounded box with a UNIFORM border is one ring node; a box
  whose sides differ cannot be, so its corners stay square and that is counted.
- Radii take CSS's overlap clamp: when two radii on a side exceed it, ALL radii scale by
  the same factor (a 200 px radius on a 220×60 box becomes a pill).
- **`aspect-ratio`** gives the height when the width is definite and the height is auto;
  an explicit height wins.
- The two dotted-border goldens changed **notation only** (`r96` → `r96,96,96,96`),
  proven by diffing with the radius fields stripped before re-blessing — after a first
  attempt compared them through the MARKDOWN renderer and showed an empty scene.

**Update — grid, matched 42 → 48 (rows 26-31), exercised 46 → 52.**
- Placement is one pass over the items in document order: a definite line is honoured,
  everything else is auto-placed at the first free slot at or after the cursor.
  **A definite MINOR position still needs its MAJOR axis searched** — skipping that
  search stacked a whole row of items on top of each other at row 0.
- Track sizing: bases by track kind (length, percentage, `min-content`, `max-content`,
  `auto`, `minmax()`, `repeat()`), then `fr` shares the free space.
- **Grid §12.8 "Stretch auto Tracks" applies unconditionally.** The profile admits no
  content-distribution property for a grid container (§3.5 has no grid `justify-content`),
  so the distribution is always the initial `normal`. Without this rule an implicit
  `auto` column — `grid-template-columns: none`, the row's own initial value — is zero
  wide and **every item in it is invisible**, which is what the first render of the
  fixture showed.
- Alignment: `justify-items`/`align-items` with `-self` overrides. **A definite specified
  size beats the intrinsic one** — an empty `width: 40px` item was being aligned as if it
  were 0 wide.
- Two of the faults found in review were in the TESTS, not the layout: `stretch` was
  expected to override a specified size (CSS applies it only to `auto`), and `.g > div`
  out-specified `.s`. A fixture that cannot show what it certifies is a fault:
  `29-grid-lines` needed a third explicit row before a two-row span was more than a
  claim, and it was settled by reading 54 px out of the NSG, not by looking at it.
- The review PNGs were first rasterized from the `.nsg` files instead of the fixtures,
  which renders the golden's TEXT as a document. It looks like a render. It is not one.

**Update — positioning and painting order, matched 48 → 51 (rows 2, 15, 16).**
- **`absolute`/`fixed`** are placed against their containing block — the PADDING box of
  the nearest positioned ancestor, the viewport for `fixed` (NSG has no scroll offset, so
  that is the only difference between the two). Offsets that are `auto` fall back to the
  static position; `left` and `right` together give the width, one alone gives
  shrink-to-fit; `top` and `bottom` together give the height. A box placed from the
  BOTTOM edge must be measured before it can be placed, which `measure` already does by
  rolling a trial layout back whole.
- **Painting order** is CSS 2 §9.9.1 reduced to what the profile admits. Every entry in
  the scene gets a key — the chain of enclosing stacking-context `z-index`es — and the
  final sort is stable, so ties keep tree order.
- ★ **`z-index: auto` is NOT a stacking context.** A positioned box with `z-index: auto`
  paints in layer 6, above the in-flow content around it, but its z-indexed descendants
  belong to the ENCLOSING context. Treating it as a context with z = 0 is the easy
  version and it traps them; the test that says which you built is a negative-z
  descendant of a `z-index: auto` box, which must paint BELOW its parent's in-flow
  siblings.
- ★ **A stacking context's own background is layer 1**, before its negative-z children —
  not part of the content around them. Without that rule `isolation: isolate` renders
  identically to `auto` (the negative child hides behind the background either way), and
  the fixture for row 15 cannot show what it certifies.
- **Inserting a box's background shifts every entry recorded after it.** The background
  slot is reserved before the children and filled after, so the ranges the children
  recorded stop pointing at what they painted. The symptom was a paint order exactly
  reversed; the fix moves the recorded ranges.
- Row 3's golden changed by ONE line and gained no new content: a `position: relative`
  box now paints above the in-flow content around it, which is layer 6. Diffing it
  through the `nsg-render` CLI instead of the conformance harness showed a false wall of
  changes, because that tool renders at a different viewport.
- A test that used `div div` proved nothing for a while: the profile does not admit the
  descendant combinator, so the rule was refused and every box stayed static. **Read the
  diagnostics before reading the result.**

**Update — inline-block and anonymous items, matched 51 → 52 (row 1, `display`).**
Every `display` value the profile admits is now laid out; nothing in §3.3 is counted.
- **`inline-block` is an atomic inline**: it wraps as one unbreakable unit, is sized
  shrink-to-fit against what the line has room for, and is laid out as a block only once
  alignment has fixed where its line starts. Its baseline is its last line box's, or its
  bottom margin edge when it has none — so an EMPTY inline-block sits ON the text
  baseline, which is the case the test pins.
- The line box now takes the tallest ascent plus the deepest descent, not just the
  largest `line-height`, or an inline-block taller than the text would overlap the block
  after it.
- **Loose text in a flex or grid container becomes an anonymous item**, which CSS
  requires and documents rely on. The implementation is one line of insight rather than a
  synthetic node: a TEXT handle reaching `block()` IS an anonymous block box, laid out
  with its parent's style. That is exactly right, because an anonymous box inherits and
  paints no background or border of its own (CSS 2 §9.2.1.1) — and `intrinsic`,
  `measure` and `baseline` then work on it unchanged.
- The test for it is an equivalence: the same text wrapped in an explicit `<div>` must
  place the next item at the identical coordinates. It does.
- Anonymous boxes have no style, so six `expect("styled")` sites became explicit
  "no decoration" fallbacks.
- ★ The row-1 fixture certified nothing for months: every value rendered as a bare line
  of text, so `flex`, `grid`, `table` and `inline-block` were indistinguishable from
  `block` in the picture. Rewritten so each value is visible — which immediately showed
  two empty flex items 0 px wide. That was the FIXTURE (an empty `width: auto` item in a
  row really is 0 wide), not the layout.

**Update — overflow and opacity, matched 52 → 54 (rows 14, 54); NSG 0.2.**
- **NSG gains two declaration lines and two optional node attributes**:
  `clip c<i> <x> <y> <w> <h>`, `group g<i> <alpha> [p<parent>]`, and a trailing `c<i>` /
  `g<i>` on the nodes that have them.
- ★ **They are FLAT attributes, not begin/end markers.** `order` is re-sorted for
  painting (stacking contexts), and a push/pop pair could not survive that re-sort; an
  index on the node does. For the same reason a clip is written ALREADY INTERSECTED with
  its ancestors', so a reader needs no stack.
- **The geometry is not clipped — the consumer clips.** A clipped child keeps its full
  size in the scene, so a reader of the NSG can still see what was cut off.
- `overflow` clips to the box's PADDING box; the box's own border and background are
  outside its own clip. ★ Per CSS Overflow 3 §3 `visible` computes to `auto` when the
  other axis is not visible, so **one non-visible axis clips both** — there is no
  clipping in x alone.
- **`auto` and `scroll` clip exactly as `hidden` does.** NSG carries no scroll offset: a
  document scene is the initial, unscrolled state, and scrolling is a chrome concern.
  Nothing is counted for it, because the scene is not wrong — it is the top of the box.
- **`opacity` makes a GROUP**, composited once. Multiplying the alpha into each node
  instead would let overlapping children show through each other, which is the case the
  fixture is built to show: at `opacity: 0.5` the overlap of two boxes stays uniform.
- Every one of the 52 existing goldens changed **by the version line only**, proven by
  diffing with the first line stripped before re-blessing.
- **The M0 corpus digest moves with the NSG version.** Today's host run:
  `457a130a…a798a4` over **118** documents, identical across 3 runs and a fourth under a
  different `TZ` and `LANG`. The corpus has also grown since the M0 gate (111 → 118), so
  this is not a like-for-like comparison with `4a99561…e121c`, and the VM leg has NOT
  been re-run for 0.2.

**Update — transforms, matched 54 → 56 (rows 61, 62).**
- **NSG gains `xform x<i> <a> <b> <c> <d> <e> <f>`** and a trailing `x<i>` on the nodes
  it applies to. `a`–`d` are scalars in **1/65536**, `e`/`f` a translation in 1/64 px.
  Like a clip, each is written already composed with its ancestors'.
- ★ **`f64::sin` is not usable here.** IEEE-754 specifies `+`, `*` and `/`, but NOT the
  libm transcendentals, so two platforms' `sin` can differ in the last bits — and NSG
  would stop being machine-independent, which is the one thing it must never be. The
  renderer reduces the angle to a quadrant in DEGREES (so 90/180/270 are exact) and
  evaluates a Taylor series to x¹³ in plain f64: error ~3e-14, far below the 1/65536 the
  matrix is rounded to. Pinned against the known angles and `s² + c² = 1` over the
  circle.
- **A transform cannot be composed when it is declared.** Its matrix needs the box's own
  height for a percentage origin, which is not known until the children are laid out —
  so while a box's descendants declare their transforms, the ancestor's is still a
  placeholder. The local matrices and their parents are recorded, and one pass at the end
  composes them. (The first attempt composed at declaration time and silently produced a
  child matrix equal to the child's own, which the nested test caught.)
- **Layout is untouched** (profile §3.8): the test asserts the box's rect keeps its
  original coordinates and only the node's attribute changes. A transformed box does open
  a stacking context.
- The review rasterizer paints a transformed node into a layer of its own and
  **inverse-maps** it into place — exact for rotation, and it reuses every painter
  unchanged.
- ★ **The corpus digest is not a regression signal, and I misread it as one.** The corpus
  IS the repo's tracked Markdown, including this file — so editing a spec changes the
  digest, and a run before an edit can never be compared with a run after it. What the
  gate actually proves is that **two runs over the same corpus agree**, which they do
  (`9a5576a3…`, twice, after this edit). A digest worth quoting across time needs a
  FROZEN corpus — or an INPUT digest beside the output one, which is what
  `--corpus` now prints: **same input digest, different output digest = the renderer
  changed; both different = the corpus was edited.** Today, over 118 documents:
  input `7e3631ef…`, output `970ad79f…`.

**Update — box-shadow, matched 56 → 57 (row 56).**
- **NSG gains a fourth node kind**: `shadow <x> <y> <w> <h> <rgba> <blur> [r<radii>]`,
  carrying the shadow's own rectangle — the box offset by the shadow's offset and
  inflated by its spread on every side, with each non-zero corner radius grown by the
  spread (CSS Backgrounds 3 §6.2). A separate node, so the existing attributes (clip,
  group, transform) and the painting-order sort apply to it unchanged.
- It is inserted at the slot reserved for the box's background, **before** it, so a
  shadow paints behind its own box. `visibility: hidden` paints neither.
- Blur is left to the consumer, as rasterization always is; the review rasterizer
  approximates it with three box passes at sigma = blur / 2.

**Update — gradients (rows 50-53 exercised, none matched yet).**
- **NSG gains `grad`**, a linear gradient over a `Tiling`: where it is painted (the
  border box), where the first tile goes, how big it is and how it repeats. Every stop
  carries an explicit position in 1/1024, because the renderer resolves CSS's implicit
  even distribution — a reader never has to.
- Fixed rules, since the profile admits neither `background-origin` nor
  `background-clip`: the POSITIONING area is the padding box, the PAINTED area is the
  border box.
- ★ **Rows 51-53 are exercised and deliberately NOT matched.** A gradient has no
  intrinsic size, so its tile always fills the area's height — which makes
  `background-position-y` unobservable, `repeat-y` identical to `no-repeat` and
  `repeat` identical to `repeat-x`. Two of four values in row 53 render the same either
  way. The goldens were blessed, reviewed, found unable to show what they certify, and
  **withdrawn**; these rows wait for `url()` and a real intrinsic size.

**Update — background images, matched 57 → 61 (rows 50-53).**
- **NSG gains `image <address> …`**, the same `Tiling` a gradient uses. The renderer
  never sees image BYTES: a node names the content address, and the input supplies
  `url -> (address, intrinsic width, height)`.
- ★ **§3.13 has no fallback measurement path, and this is where that bites.** An image
  the input does not declare is **refused with a diagnostic** and nothing is painted —
  the renderer never opens a file, never decodes a header, never guesses. The conformance
  harness stands in for the converter: it reads each fixture's `NN-name.subs` manifest,
  hashes the bytes for the address and takes the intrinsic size from the PNG header, so a
  fixture cannot declare a size the image does not have.
- ★ **With a real intrinsic size, rows 51-53 can finally show what they certify** — the
  reason they were withheld one commit ago. `background-position-y` moves the mark,
  `repeat-y` is a column rather than a full-height wash, `cover` crops and `contain`
  leaves a sliver. The test image is a 40×20 PNG with a blue top-left corner, a red L and
  a green bottom-right block, so any flip, crop or tile is obvious.

**Update — `<img>` and object-fit, matched 61 → 62 (row 58).**
- **A replaced element is an atomic inline** whatever its `display` says: it has no
  inline content to flow, only a box of its declared size. So `<img>` reuses the
  inline-block machinery exactly.
- Replaced sizing (CSS Sizing 3 §5.2): an auto width takes the declared intrinsic width,
  or follows the ratio from a specified height, and the same for the height — which is
  the profile's anti-CLS rule (§1.2) made concrete, because both are known before layout.
- **`object-fit` is resolved into the tile**, so a reader needs no special case: `cover`
  produces a tile LARGER than the content box, which the paint area then clips, and
  `scale-down` is the smaller of `none` and `contain`. Centring is fixed, since the
  profile has no `object-position` row.
- ★ **A trial layout must leave no diagnostics behind.** `measure` rolled back the scene
  and the counters but not the diagnostics, so an undeclared image would have been
  reported once per probe.
- Only rows 36 (`font-style`, which needs an italic face in the pinned set) and 48
  (`direction`, which needs bidi) remain unmatched.

**Update — italic faces, matched 62 → 63 (row 36); FONT SET VERSION BUMP.**
- The pinned set gains four faces: **IBM Plex Sans Italic** (variable, one file for 400
  and 700) and **IBM Plex Mono Italic / Bold Italic** (OFL 1.1, unmodified upstream).
  Real letterforms, never a shear — a sheared sans is not an italic, and prose uses
  emphasis constantly.
- ★ **Mono needed BOTH italic files.** Mono has no weight axis, so instancing the regular
  italic at 700 returns the same bytes: a "bold" that is not bold. The canonical address
  said so — the two entries had one address between them, which is exactly the kind of
  lie content addressing is supposed to catch.
- The italic stack falls back **upright** rather than losing a glyph: Plex italic, then
  Plex upright, then DejaVu (which ships no italic). A substitution is counted in
  `report.em_upright`, whose old meaning — "italic drawn upright", then always true — was
  retired with the counter it justified.
- **A font-set change changes every golden, by design.** All 62 re-blessed, verified to
  differ ONLY in the `font` declaration lines and the mechanical face renumbering
  (DejaVu moved from f4/f5 to f8/f9), with a script rather than by eye.
- **The font set version is part of the normalizer's cache key** (profile §3.13), so this
  is a deliberate version bump. Corpus after it: input `c4f3aa0c…`, output `59b464bf…`
  over 118 documents — the output moved because the repo's own prose is full of emphasis
  that is now genuinely italic.
- Only row 48 (`direction`) is left.

**Update — bidi, matched 63 → 64 (row 48). THE NUMBER IS COMPLETE: 64/64 exercised,
64/64 matched.**
- Full UAX#9 through `unicode-bidi` (MIT/Apache-2.0): `direction` gives the paragraph's
  base level, each line's items are resolved to embedding levels, reordered by rule L2
  (reverse every contiguous run at or above each level, highest down to the lowest odd
  one), and `text-align: start`/`end` map to the base direction's edges.
- **A folded space becomes an item of its own** when a line needs reordering. Spaces are
  normally folded into the following word's offset, which is fine going one way, but
  reordering moves what is BETWEEN words — a space glued to a word ends up on its far
  side and shifts the line.
- A word whose own text spans two levels is **split and re-shaped** at the boundary,
  which is itemization: what a real engine does before shaping, not after.
- ★ **The control is that LTR output cannot move.** Only a line with RTL content or an
  RTL base is touched, and all 63 existing goldens stayed BYTE-IDENTICAL across the
  restructure — no re-blessing, so the claim is checked rather than asserted.
- Verified by reading coordinates out of the NSG, not by looking: in an LTR paragraph the
  first logical Hebrew word sits to the RIGHT of the second; in an RTL paragraph the
  leading Latin run is pinned to the right edge and stays internally LTR; and a number
  inside an Arabic run stays LTR.
- ★ **The unread-row control had to be rescued.** It worked by naming a row the layout
  did not read — and there is no such row left, so it would have passed while testing
  nothing. `count_rows_outside` now takes the read set, and the test hands it one with a
  row held out.

### 8.4 M2's CORPUS leg — first reading (2026-09-23)

`nsg-render --corpus-html <dir>` renders every document through the profile renderer and
prints what each one refuses. Over **29 real pages** fetched on 2026-09-23 (Wikipedia,
MDN, WHATWG, W3C, rustdoc, FreeBSD docs, blogs, HN/lobste.rs, and info.cern.ch) and
converted by `navigator-prerender` (19 tier 2, 10 tier 1):

> **CORPUS: 1/29 documents render with NO refusal. 0 unimplemented counts, corpus-wide.**

★ **The two numbers say opposite-looking things and both are true.** The renderer
implements everything Profile v1 admits — that is the 64/64 and the zero unimplemented
counts. Real documents are not written in Profile v1 — that is the 1/29. **The gap is
the normalizer (§6), which does not exist.** The refusal histogram IS its specification:

| hits | docs | code | whose job |
|---|---|---|---|
| 1726 | 3 | `value.var-invalid` | normalizer: resolve custom properties |
| 1215 | 21 | `input.style-attribute` | normalizer: rewrite inline styles into rules |
| 795 | 11 | `selector.unadmitted` | normalizer: flatten descendant combinators — the hard one |
| 201 | 10 | `property.shorthand` | normalizer: expand to longhands |
| 172 | 25 | `input.stylesheet-not-supplied` | CONVERTER: it does not supply external CSS at all |
| 131 | 19 | `input.subresource-not-supplied` | converter: declare intrinsic sizes (§3.13) |
| 126 | 14 | `table.column-width-undeclared` | normalizer: pre-measure columns (§3.13) |
| 74 | 5 | `property.unknown` | genuinely outside the 64 rows; must be dropped |
| 65 | 2 | `important.excluded` | normalizer: cascade `!important` away |
| 42 | 10 | `value.invalid` | mixed |
| 29 | 8 | `media.unadmitted` | normalizer: evaluate and flatten `@media` |
| 27 | 4 | `at-rule.unadmitted` | normalizer: flatten or drop |

★ **These are a LOWER BOUND.** `input.stylesheet-not-supplied` fires on 25 of 29
documents, which means most of the corpus's CSS never reached the renderer — nearly every
other count above comes from inline `<style>` blocks alone. Supply the external sheets and
the numbers go UP, not down.

**What refusals cost is presentation, not content.** A refusal drops a declaration; it
never stops the document. The W3C flexbox spec refuses 1071 times and still renders as a
readable, correctly structured document in UA defaults; the WHATWG HTML spec (5 refusals,
both "not supplied") renders with correct nested lists and links; info.cern.ch (1990, no
CSS) is the one document that refuses nothing. The renderer is not the bottleneck.

**Reproducing:** the corpus is not in this tree (fetched pages change). Assemble a
directory of saved pages with a `manifest.tsv`, then:

```
PRERENDER_EMIT_DIR=<emitted> PRERENDER_EXPLORE=1 prerender <corpus>
# extract each recording's "document" field into <converted>/*.html
nsg-render --corpus-html <converted>
```

### 8.5 The normalizer — M2's corpus leg CLOSED (2026-09-23)

`navigator-normalize` is Profile v1 §6's component: it accepts real-world HTML and emits
a profile-conformant document. Over the same 29 documents §8.4 measured:

> **1/29 → 29/29 documents render with NO refusal**, with each page's real external
> stylesheets applied.

**The design decision.** It **resolves the cascade itself** and emits one flat class per
distinct declaration block. It does NOT rewrite each unadmitted construct into an
admitted one — impossible in general for a descendant combinator. It evaluates the
selector, keeps the result, and throws the selector away. Eleven kinds of unadmitted
selector, `!important`, shorthands, `var()`, `@media`, `@import` and inline `style=`
attributes collapse into that one move, which is why the 692 descendant combinators cost
no more than the 3 `+` combinators.

**The gate is the profile itself** (§6: "its output is checkable"): every test renders the
OUTPUT and asserts zero refusals, each with a control showing the input IS refused first.
The normalizer also checks each declaration against the profile's own grammar before
emitting it, so `value.invalid` from the renderer is impossible by construction.

★ **Three bugs the RENDER found, none of which a refusal count would have shown.** All
29 documents were already at zero refusals when each was found:
- **Custom properties taken as "the last `--x` in the file"** pick up whatever a
  `@media (prefers-color-scheme: dark)` block set, and **every page rendered in its dark
  palette**. They belong in the cascade, per element and per media context, inheriting;
  and a base rule using `var()` must be **re-resolved in each context whose customs
  differ**, since that is how dark mode reaches `background: var(--bg)`.
- **A `<link media="…">` conditions the whole sheet.** Ignoring the attribute applies a
  dark-mode or print stylesheet unconditionally.
- **`@import` is where the real CSS usually is.** The W3C's stylesheet for a spec is 123
  bytes: one `@import "base.css"`. A strict parser drops the at-rule, and the page renders
  exactly as if the sheet had never been fetched — which is what "29/29, no refusals" looked
  like for an hour.

**Renderer changes this needed**, both of them §3.13 finally landing in full:
- A table's first row may declare `min-width`/`max-width` per column — the min-content and
  max-content widths, measured offline by `premeasure_tables` with the pinned font set —
  and the layout distributes them with the SAME track sizer grid uses. An exact `width`
  still means exact; only measured columns take leftover space, and only when the table's
  own width asks for it.
- An `<img>` carrying `width`/`height` **has declared its intrinsic dimensions** (§1.2).
  The subresource map names the BYTES, which is a different thing: an image whose size is
  declared but whose bytes were not supplied reserves its space — no layout shift — and is
  counted, not refused.

**What it drops, it reports**, by reason and count: properties outside the 64 rows
(`float`, `cursor`, `transition`), pseudo-element content, layered backgrounds, images
whose intrinsic size the input never declared. Silence would be the only real failure.

**The lane is closed (2026-09-23).** The converter now carries what the normalizer needs,
so nothing in the chain fetches or measures out of turn:

> **converter recording (`atrium-navigator-recording/3`) → normalizer → renderer:
> 29/29 documents, zero refusals, no sidecars.** 167 stylesheets and 140 measured images
> travel in the 29 recordings.

- **`@import` is followed** to the profile's depth, keyed as the importing sheet names it
  — an import keyed by ABSOLUTE url while the normalizer resolves it RELATIVE to the href
  as written is a lookup that misses, and the CSS goes silently absent.
- **A `<link media="…">` travels with its sheet**, so a dark-mode stylesheet stays
  conditional.
- **Image intrinsic sizes come from the image's own header** (PNG, JPEG, GIF, WebP, and
  SVG by attribute or `viewBox`), and the bytes are named by **content address**, so the
  renderer paints from CAS without anyone re-fetching. An image the document already
  declares needs no fetch; one whose bytes cannot be had still reserves its space when the
  size was declared. `Fetcher::get_bytes` exists because a header read through a lossy
  UTF-8 conversion is not a header.
- Two value-level rules the corpus forced: **nothing still holding `var()` is ever
  emitted** (Wikipedia defines `--font-size-medium: var(--font-size-small)` and the
  reverse in different scopes, which merges into a cycle here), and a media feature
  written `calc(640px - 1px)` is **constant-folded** rather than dropping a whole
  responsive breakpoint.

**Images paint (2026-09-23).** The converter writes each fetched image into a
content-addressed store (a stand-in for Tessera CAS: the name IS the hash, so a second
document referencing the same image costs nothing), the recording names the address, and
the renderer's subresource map is built from the recording — `src -> (address, intrinsic
size)`. The document declares the SIZE so layout never waits; the recording names the
BYTES so paint never fetches. 139 of the corpus's images paint.

★ **Four bugs, three of them found by LOOKING at the render:**
- **The scene's extent was the root box, not the content.** Wikipedia sets
  `html, body { height: 100% }`, so the extent said 600 px while 14,000 nodes sat below
  it. The extent is what a reader can scroll to, which is the content.
- **`measure` rolled back rects, runs and links but not shadows, gradients or images.**
  The scene still rendered correctly and quietly carried orphan nodes that nothing
  pointed at — found by a test that counted nodes, not by anything visual.
- **Measurement and layout disagreed about a replaced box.** `replaced_size` consulted
  only the subresource map, so during offline table pre-measurement — which runs without
  one — an image measured 0 wide and its column came out too narrow. The DECLARED
  `width`/`height` is the contract (§1.2); both paths must read it.
- **CSS 2.1 §17.5.2: a table's used width is the GREATER of its specified width and its
  minimum content width.** Clamping the columns to a narrower specified width instead
  left a 330 px image hanging 28 px outside a 310 px infobox. The user saw it in the
  render and reported it twice before I stopped guessing and measured the rects.

★ **The first of those fixes did not appear at all**, because `nsg-raster` is
feature-gated and `cargo build --release` does not rebuild it — the same stale-review-tool
trap this file already records.

**Six documents reviewed by eye (2026-09-23), five bugs.** None of them moved the refusal
count, which stayed at 29/29 throughout:
- ★ **Block-in-inline.** An inline box holding block-level content was flattened into one
  line box, so **Hacker News — whose whole page is wrapped in `<center>` — rendered as a
  single paragraph**. An inline element containing block content is promoted to
  block-level, in one bottom-up pass so it stays O(n).
- **`<noscript>` is RAW TEXT when scripting is enabled.** Adding it to the UA block list
  put a literal `<iframe src="toc.html">` on the Rust book's page. The converter ran the
  scripts, so the fallback is `display: none`.
- ★ **Logical properties.** `padding-block` alone appears **5024 times** in 29 documents,
  and mdBook's page layout hangs on one `margin-inline-start`. The normalizer maps them to
  physical ones; dropping them as "unknown" silently removes the layout.
- ★ **A state belongs to the element it is written on.** `#t:checked ~ .p` styles the
  SIBLING, so emitting `:checked` on the sibling names a state it can never have. Static
  states are decided from the DOM; a dynamic one on a non-subject compound is refused and
  reported. `:visited` is always false — a document has no history, and claiming one would
  leak what the reader has read.
- **The normalizer discarded the root's attributes**, by wrapping the serialized body in a
  hardcoded `<html>`. `lang` and `dir` decide language and base direction.

**A snapshot has to carry interactive state.** HTML deliberately does not reflect
`input.checked` into the attribute — the attribute is the default, the property is the
state — but the converter's output is a STATIC document and CSS reads `:checked`. A
sidebar or menu toggled open by script rendered closed. `checked`, `selected`, `open` and
`disabled` now reflect into the snapshot.

★ **Open, and a profile question:** `min()`, `max()` and `clamp()` are not admitted inside
`calc()`. Modern CSS uses them constantly — mdBook's `--sidebar-width: min(…, 80vw)` means
its whole page layout drops — and they are as bounded and deterministic as `calc()` is.
Admitting them is a Profile v1 change, so it is the user's call, not this file's.

**Six more documents (2026-09-23), six more bugs**, the count again unmoved at 29/29:
- **`nowrap` beats `break-word`.** `overflow-wrap` applies only where breaking is allowed
  (CSS Text 3 §5.5), and under `nowrap` there is nowhere. MDN's nav buttons rendered one
  character per line, vertically down the page.
- ★ **`display: contents` is a STRUCTURAL instruction, not a value.** The element
  generates no box and its children take its place, so the normalizer carries it out on
  the DOM — 1510 elements in 29 documents — and the profile needs no such value.
- **Media range syntax.** `@media (width <= 1044px)` is how modern sheets are written and
  the profile's parser knows only `max-width`, so every rule inside was dropped, including
  the `display: none` that hides MDN's mobile menu. Translated, with a 0.02 px nudge for
  strict comparisons so the boundary falls on the honest side.
- ★ **Named grid areas resolve to numbered lines.** `grid-template-areas` plus
  `grid-area: toolbar` is a static mapping, so the normalizer computes it; without it
  every child lands in the same cell, and rustdoc's breadcrumb rendered on top of its
  search box.
- **A `<select>`'s options are the content of a CONTROL, not of the page.** With no
  controls yet (§3.14), dumping every option as body text is strictly worse than showing
  none: FreeBSD's man page rendered its whole version dropdown, 200 releases, as a
  paragraph.
- **`font-size: 0` paints nothing.** It is how a page hides text from sight while keeping
  it for a screen reader; emitting the run anyway put rustdoc's hidden "Copy item path"
  into the scene, and a consumer scaling glyphs by zero drew them at the font's own
  units — a grey blob 450 px across.

**The normalizer gained an exit gate**: every declaration is checked against the profile's
grammar on the way out, whatever path it took to get there. One check at the exit is worth
more than trusting every entrance.

**MDN's app shell — six bugs (2026-09-23), one of them general.** Chased with a new
`NSG_DUMP_BOXES` review mode, because the scene says what was painted and never which
element painted it, which is the question a layout investigation starts from.
- ★★ **Track lists were never converted to px.** Every other value type is converted in
  the cascade; `V::Tracks` was missed, so `minmax(15rem, 1fr)` was read as fifteen
  PIXELS. MDN's 48rem content column came out 48px wide — **every em/rem grid on the web
  was sized at a sixteenth of its intended value**. The conformance fixtures are written
  in px, so 64/64 could not see it.
- ★ **A track has TWO sizing functions and they are not interchangeable**: the minimum
  gives the base size, the maximum a growth limit. Taking the maximum as the base made
  `minmax(0, 48rem)` claim 768px of an 800px grid before anything else was sized.
  Rewritten as CSS Grid §12.4-12.8.
- An `fr` inside `minmax()` is still flexible, and a flexible track with INDEFINITE free
  space sizes to its content (§12.7.1) — returning zero gave a page-tall row a height of
  zero and every section below it was painted on top of the one above.
- **A row flex container is as wide as its items TOGETHER.** `intrinsic` took the max,
  which is right for stacked blocks and wrong here: the breadcrumb measured as one crumb
  wide and the rest was clipped.
- **`display: contents` must be spliced BEFORE named areas are resolved**, or a child's
  parent is still the box that generates none — MDN's header, body and right sidebar sit
  inside a `display: contents` `<main>` and so never found the grid's template. Named
  areas resolve per MEDIA CONTEXT too, since a responsive page keeps its layout there.

**Previously recorded as a known limit; now resolved.** The note below is kept because the
shape of the mistake is worth keeping: the shell overlapped  and I attributed it to
`position: sticky`, `!important` overrides and container queries — a guess from reading
the stylesheet rather than measuring the boxes. Not one of those was the cause. Everything
reviewed — Wikipedia, the W3C and WHATWG
specs, the Rust book, rustdoc, Hacker News, lobste.rs, danluu, Joel on Software, the Rust
blog, GitHub, freebsd.org, the FreeBSD man page, the GNU coreutils manual, info.cern.ch —
renders as the site itself looks.

**Six more documents (Wikipedia's bidi article, the W3C grid spec, the Rust API
guidelines, RFC 2616, GitHub, Unicode TR9), one bug.**
- ★ **A masked box is drawn THROUGH its mask**: the colour is the ink, the mask is the
  shape. The profile admits no mask, and painting the fill unmasked turns every icon into
  a solid square — Wikipedia's logo and search icon came out as two black blocks. The
  normalizer now drops the background of a masked box and says so, because painting
  nothing is closer to the page than painting a black square.
- Unicode TR9 renders its bordered two-column table from offline pre-measurement, and its
  Devanagari editor name shows tofu: an honest coverage gap in the pinned font set,
  counted as `notdef`.
- GitHub renders its header, breadcrumb and action buttons; the "Uh oh! There was an
  error while loading" is GITHUB's OWN message, because its React file tree never ran in
  the converter. A converter fidelity limit, not a rendering one.

**GitHub, looked at properly: four bugs, and none of them was GitHub's.** The buttons
came out underlined, stacked one per line, colourless, and the whole page squeezed into
353 of 800 px. Each had an ordinary cause, and each is general.
- ★★★ **A custom property's name is CASE-SENSITIVE** (CSS Variables 1 §2). The normalizer
  lowercased every declaration name, so `--fgColor-default` and `--button-default-fgColor-rest`
  — and every other camelCase design token in Primer, and in anything else built on
  design tokens — resolved to nothing. And it failed SILENTLY: a `var()` with no
  definition and no fallback left an EMPTY value, which reads exactly like a declaration
  nobody wrote. GitHub's buttons lost their colour, their background and their border,
  and the page header its black bar. `var()` with no definition and no fallback is now
  kept as-is so the drop is REPORTED, per §6's rule that nothing goes missing quietly.
- ★★ **Cascade layers were dropped with their contents.** Primer, Bootstrap 5.3+ and
  Tailwind v4 wrap their whole stylesheet in `@layer`, so dropping the at-rule drops the
  stylesheet: `a { text-decoration: none }` never reached the cascade. The normalizer now
  parses `@layer` (both the statement that fixes the order and the block), and ranks by
  CSS Cascade 5 §6.4.4 — unlayered wins, later layer beats earlier, and `!important`
  reverses all of it.
- ★ **A ROW of same-direction floats is a horizontal strip.** The profile has no floats
  and never will, but block-stacking a float row is simply wrong. When every in-flow
  element child of a parent floats the same way and the parent holds no text of its own,
  the normalizer lays them out as `inline-block`, which puts them in the same places. A
  lone float beside text is NOT a strip — the text is meant to wrap — and keeps being
  dropped; so does a floated `td`, because CSS 2.1 §9.7 says float does not apply there.
- ★★ **A flex item's intrinsic width must stop at a child's definite width.** Measuring
  past one sized GitHub's collapsed file-tree pane — whose own `width: 0` is what
  collapses it — at the max-content width of the tree inside: 446 px of empty space,
  with the README squeezed into the remaining 353.

**doc21 (Joel on Software), one more bug — and it is the `.screen-reader-text`
idiom.** ★★ **An absolutely positioned INLINE element is out of flow.** It is blockified
(CSS Display 3 §2.7) and contributes nothing to the line, but `collect_inline` flattened
it into one — so its own `width: 1px; height: 1px; overflow: hidden`, which is the entire
mechanism of the visually-hidden idiom used on every accessible site, never applied. Two
labels came out as "View menu" and "View sidebar" written across the page. Such a box is
now set aside and placed after the line it was written in, taking that line's origin as
its static position.

★★ **Wikipedia's table of contents overlapped the article title — a `max-height` on a
clipping box is an upper bound.** The TOC lives in `max-height: calc(100vh - 48px);
overflow-y: auto`. The clip was built from the box's DEFINITE height, which `max-height`
is not, so the clip was unbounded and all 1390 px of contents painted straight over the
article below. A `max-height` now bounds the clip even when the height is auto — the box
may end up shorter, but nothing can be painted past that line. (It had been wrong since
before this session; verified by rebuilding the parent commit in the same tree. Earlier
in this log, "renders as the site itself looks" was too generous about doc01.)

**The last seven unreviewed documents (Wikipedia ×2, MDN ×3, danluu, example.com): four
bugs, all general.** With these, all 29 corpus documents have been looked at.
- ★ **A decoration is not drawn across an atomic inline** (CSS Text Decoration 3 §2.1).
  An inline-block's text is underlined by its own line boxes; the line holding it ALSO
  drew one across its whole margin box, so every MDN table-of-contents link came out
  underlined through its padding — "_Try it_".
- ★★ **An inline-block's min-content contribution is its OWN min-content.** It wraps
  inside itself; measuring it as an unbreakable slab at its max-content width set the
  minimum of MDN's contents column by its longest entry, 26 px wider than the column.
- ★ **A decoration extends only over contiguous decorated items.** Matching open
  underlines by colour alone bridged two links on one line into one underline through
  the plain text between them: "CSS property sets how the content of a replaced element",
  all underlined.
- ★★ **The canvas takes the root's background, or the body's** (CSS Backgrounds 3
  §2.11.2). example.com is `body { background: #eee; width: 60vw }` — a grey PAGE,
  which rendered as a grey column on white. The body's own box now paints nothing and
  the canvas fill spans the whole document.

**Wikipedia's contents at 800 px — two bugs, one in each stage before the renderer.**
Real Wikipedia hides the sidebar contents below 1120 px, behind a button; the lane
showed them above the article. The hiding rule is
`@media screen and (max-width: calc(1120px - 1px)) { .client-js .vector-sticky-pinned-container { display: none } }`,
and neither half of it reached the renderer.
- ★★★ **The converter: `className` was a SNAPSHOT, not a reflection of the class
  attribute.** `el.className = "x"` set a plain JS property and never touched the
  element. Wikipedia's first inline script, `document.documentElement.className =
  "client-js …"`, ran with zero errors and left `client-nojs` in every Wikipedia
  recording — so no `.client-js` rule ever applied. The same silent drop took the
  classes off every element a script built with `className`: mdBook's code-block
  buttons, rustdoc's search container (an empty bordered box at the end of the page,
  now gone), WordPress's live regions. Seven of 29 recordings changed.
- ★★ **The normalizer: a greedy paren trim, surviving in a second place.**
  `trim_matches('(' | ')')` ate the closing paren of `calc(1120px - 1px)`, and the
  query was dropped with every rule in it — 18 of Wikipedia's narrow-layout blocks. It
  had already been fixed once, in the range-syntax path. Queries are now split on `and`
  at paren depth zero and each part admitted on its own, which also admits compound
  breakpoints (`(min-width: …) and (max-width: …)`) and the doubled prefixes
  concatenated sheets leave (`screen and all and (…)`). One refused part still refuses
  the whole query.

**The corpus widened to 96, with a flagger instead of eyes.** `nsg-flag` (review
tooling, `navigator_render::flag`) renders every normalized document in a CHILD process
with a time limit and measures the geometric symptoms every reviewed bug had shown: text
over text, text crossing the canvas edge, text sliced by a clip, all text in a strip of
the page, and far less text painted than the document holds. Each check has a positive
AND a quiet control. Of the 96 fetched documents, 78 render: 12 did not convert within
30 s, and 6 are an origin's bot check (the converter's verdict), plus FT's, which its
conservative detector correctly declines to call (the page has a footer).
- ★★★ **A renderer HANG on a 6 KB page.** An item placed past the explicit grid (FT's
  `dt`/`dd` in columns 1 and 2, no template) never found a slot: the fixed axis was
  sized from the template alone, and the placement search advanced the row forever. The
  implicit grid is now sized first (CSS Grid §8.5). A per-document jail bounds the blast
  radius; it does not make an infinite loop acceptable.
- ★★ **The converter published challenge pages.** `origin_refusal` was set and reported
  — and the recording written anyway, because the emit path never read it. Six "Just a
  moment…" pages (Stack Overflow, Super User, B&H, AP News, The Economist, timeanddate)
  would have been published as the sites.
- ★★ **HTML's own hiding was missing from the UA sheet**: no `[hidden]`, no closed
  `<dialog>`. arXiv's closed citation modal greyed out the whole abstract page.
- ★★ **Bare text in a flex container is an anonymous flex item.** The row-flex intrinsic
  width summed element children only, so arXiv's header links — `display: flex` anchors
  holding bare text — measured zero and piled onto each other.
- ★ **A table cell's `width` is a floor, not a cap** (CSS 2.1 §17.5.2.2). The profile
  reads a first-row width as exact, so the normalizer now folds the author's width into
  the measured pair: the W3C specs' `th { width: 3em }` held their property tables'
  header column to 48 px, and "Initial:" painted over its value.
- ★ **Media queries, three more ways.** A boolean feature (`(prefers-reduced-motion)`)
  is rewritten to the value it stands for; an unexpanded Sass function
  (`getbreakpointdownvalue(48em)`) or a negative bound is dropped, never passed through
  to be refused; and the `and`-splitter, rewritten earlier the same day, panicked on the
  first query holding a multi-byte character — the corpus caught my own regression.

Result, 78 documents: refusals 2 → 0, overlaps 16 → 9, crossing the edge 14 → 13, hangs
1 → 0. OPEN, each needing more than a patch: `colspan` is not in the profile, and a
first row that spans (every Wikipedia navbox) declares too few columns — a profile
decision; horizontal carousels (TikTok, Spiegel) stack their slides; Asahi's content is
shifted off the left edge; outside list markers at a zero-padding edge (Hacker News).

**`colspan` admitted to the profile (2026-09-24)** — see profile §3.9 and §3.13. The
renderer places each cell at a running column index and gives a spanning cell its
columns' widths plus the spacing between them; columns are declared by `<col>` (a first
row that spans cannot declare them, and without `<col>` such a table is refused). The
producer settles a spanning cell's shortfall over its columns. Two corrections found by
the corpus, not the tests: `<col>` survives parsing only inside a real `<table>`, so a CSS
table (a `div`/`ul` with `display: table`) still declares on its first row — Wikipedia's
portal box and a W3C layout table were REFUSED until it did; and only an HTML cell
spans, so `colspan` on a `div` is ignored. 153 spanning cells in 9 of 78 documents;
Wikipedia's navboxes now render as the site does, less the `::after` separators.
78 documents: 0 refused; crossing the edge 13 → 10. `rowspan` stays out (44 cells in 3
documents), a decision recorded with its numbers.

**An oracle: the Web Platform Tests (2026-09-24).** `wpt-reftest` (navigator-normalize,
feature `wpt`) runs WPT's CSS reftests through the real lane — normalizer → profile
renderer → pixels at 800×600 — on both the test and its reference, and compares pixels
within the test's own declared tolerance. No browser is needed: a reftest's reference is
the expected rendering, written in simpler CSS. Its controls (an identical pair passes, a
red/green pair fails on exactly its 5,000 px, a `mismatch` pair passes) run first. Tests the
oracle cannot judge fairly are skipped WITH the reason: they run script (the next step is
to pass them through the converter, as the lane does), need the Ahem test font, or use a
non-PNG image — the review rasterizer paints PNG only, which is also why SVG logos were
blank in every corpus review. WPT lives at `~/src/wpt` as a sparse clone (12 directories,
~12 MB), BSD-3-Clause, never committed here.

First findings, all fixed: absolutely positioned boxes ignored auto margins (CSS 2.1
§10.3.7/§10.6.4 — the `left: 0; right: 0; margin: auto` centring idiom); a border box could
be shorter than its own borders (the height half of the non-negative content box); `.xht`
references were read as HTML, so CDATA-wrapped CSS vanished; and the converter sized an
SVG from the first `width=` anywhere in the file and dropped a width-only SVG, where CSS's
default object size gives 150 px of height. `css-ui`: 56 → 65 pass of 88 evaluable, no
regressions; box-sizing: 4 of 4 evaluable (19 need SVG painting). OPEN, a profile question:
an image declaration is (width, height), which always implies a ratio; a width-only SVG has
none, so under `max-height` we shrink its width where a browser keeps it.

★ **Two renderer tests had been failing for hours** — a stale sample golden and a pinned
`nsg 0.1` header — behind `grep -c "test result: ok"`, which counts the suites that passed
and cannot see one that failed. Counted properly: **505 tests, 0 failures, six crates**.

**Margin collapsing compiled; the oracle stops measuring the checkout (2026-09-25).** The
normalizer now collapses margins into literal ones (profile §7a, `collapse.rs`); the
renderer is unchanged. Against the last commit, over css-ui and CSS2 margin-padding-clear,
normal-flow, positioning, floats-clear and tables: **860 → 906 pass, 56 fixed, 10 lost** —
and all 10 lost were ACCIDENTAL passes involving floats: `padding-applies-to-*` compares a
table to a `float: left` reference, and both used to fill the line, so they matched.
CSS2's margin-collapse tests: 5 → 14. Corpus flag verdicts unchanged over 85 documents;
140,430 margins rewritten; the Rust book 1,656 px shorter. The oracle caught three of my
own mistakes before they landed: an absolute box with auto offsets is placed AFTER the
margins before it (so that part of a chain is pinned in front of it); an empty box's empty
descendants collapse through it too; `max-height` separates a parent's bottom margin.

★ **22 baseline passes measured files that were never fetched.** WPT's CSS2 `support/`
directory is outside the sparse clone, so a reference's `black96x96.png` painted nothing —
and a test that ALSO painted nothing (a zero-width table cell) "matched" it. `wpt-reftest`
now skips a test whose test or reference names a missing file (`SKIP:missing-file`,
454 over these directories) instead of comparing; `--save` also writes both normalized
documents. The fix to the zero-width cell exposed renderer bugs: an auto-width table
filled its line instead of shrink-wrapping (CSS 2.1 §17.5.2.2), a specified table width
went unshared when every column was empty, and a definite-width block's border was counted
twice in its container's intrinsic width.

### 8.6 M3 — PASSED (recorded 2026-11-03)

Every leg of the gate was met by work already described above; this entry records it.

- **Worker capabilities == none.** The corpus ran through real FreeBSD jails with
  `Confinement::Launcher` → `portcullis exec`, the worker an installed, signed app whose
  manifest declares no capabilities at all: 98 sessions, 259 navigations, 259 rewinds,
  0 failures, and no jails, mounts or roots left behind (§4.7a).
- **Validator fuzzed with reached-coverage reported.** `tests/fuzz.rs` fails unless every
  rejection and every note the validator can emit was produced by a generated input (§4.7b).
- **Per-document jail launch cost measured.** About 25 ms to open and 26 ms to close a
  document's jail, navigation unaffected; one jail per document stays the default (§4.7c,
  §10 q1).

### 8.7 M4 — forward (2026-11-03)

`History::forward` re-takes the step the reader last backed out of. It stores the
**transition**, not the document it led to: after a back, the document is byte-identical
to the one the step was first taken from, so the same transition applies with the same
preconditions and its result is checked exactly as the first time. A stored document would
be a second copy of a derivable fact. A new step after a back clears what was ahead, as in
every browser, and `Forward::AtEnd` answers when nothing is. A refused forward keeps its
place, as a refused back does.

It runs through every layer: `Session`, `DocumentHost` (in-process and jailed — the worker
answers `FWD`), and the broker's `Request::Forward` / `Event::Advanced`. Tests: byte equality
through three mixed steps and back, a new step replacing what was ahead, forward at the end,
the broker driven through its control plane only, a vanished session, and the full cycle
across a worker process. `roundtrip.rs` gains a corpus check (go, back, forward, byte-equal)
that runs with `NAVIGATOR_RECORDINGS` set, like the others.

### 8.8 M4 — session restore (2026-11-03)

**A saved session is its recording's address and the steps taken, nothing else.** The
document is derived by replaying the steps through the ordinary apply path, so a saved
record holds no second copy of anything derivable. `Session` keeps the trail as indexes
into the recording's transition table (and the steps backed out of, for forward); a step
that is not in the table makes the session unsaveable, and saving says why rather than
writing a record that cannot be replayed. The trail is capped at 4,096 steps.

**The record is untrusted, like a recording.** `Saved::from_json` bounds the bytes before
parsing (128 KiB), types every field and defaults none. `Sessions::restore` opens the
recording through `open` (profile and budget checks included), refuses an index out of
range or unanchored, replays every step with its preconditions, rebuilds forward by taking
the steps backed out of and backing out of them again, and then requires the document to
hash (BLAKE3) to the one saved. Any refusal closes the half-built session.

**The store is content-addressed** (`store.rs`): `MemoryStore`, and `DirStore`, one file per
object named by its address, the shape of the converter's `PRERENDER_BLOB_DIR`. A `get`
re-hashes and refuses a mismatch, and an address is validated before it becomes a path. The
broker gains `Request::Save` / `Event::Saved` and `Request::Restore`, which answers
`SessionOpened` like an open. `InProcessHost::with_store` puts each recording into the
store when it is **opened**, because by the time a session is saved the bytes are gone. A
host without a store, including the jailed one for now, answers `Blocked` with the reason.

Tests (`tests/restore.rs`, 11): a restored session matches byte for byte and walks the same
back and forward path; the record round-trips its encoding; a step out of range, a step that
no longer applies, and an end state not reached are each refused with nothing left open; a
step outside the recording makes a session unsaveable; malformed and oversized records are
refused; a store refuses a replaced object and a path-shaped address; and through the broker
a saved session outlives its expiry and comes back with forward and back intact.

**What a restore does not reproduce exactly:** how many early states a bounded history had
forgotten. A replay under the same limits forgets by the same rule, which can differ from a
session whose steps and backs were interleaved.

**Through the jailed host (2026-11-03).** The worker holds no store, so a saved record is
split in two. A `Trail` (url, steps, ahead, document digest; `atrium-navigator-trail/1`) is
the worker's half: `SAVE` answers it, and `RSTR` replays one onto the document the worker
has just opened. The recording's address is the broker's half, because only the broker
holds the store. `JailedHost` stores the recording at open, as the in-process host does. On
save it parses the worker's trail strictly (the worker may be compromised) and completes
the record. On restore it starts a fresh worker on the stored recording and sends it the
trail. Any failure closes that worker.

A lying worker can at worst save a trail that will not replay, and restore refuses that at
the first bad step or at the digest. The worker now runs the same `Sessions` code as the
in-process host (with one session), so navigation, the trail and the replay cannot drift
apart between the two hosts.

Tested unconfined in `tests/jailed.rs`. `examples/jailed_corpus` now saves every session
that navigated, closes it, restores it, and checks that the restored session saves to the
same address. So `navigator-jail-e2e.sh` exercises restore inside a real jail.

**Real jail, 2026-11-03 (7f45ab3).** The run used `navigator-jail-e2e.sh` in a FreeBSD
16-CURRENT arm64 VM under TCG, with no KVM, so timings are about 70x slower than native.
The launcher was confined, the broker ran as an unprivileged user, and portcullisd created
one jail per document. Result: 1 session, 1 navigation, 1 rewind, 1 restore, 0 failures.
The restored session saved to the same address. Open took 3.6 s, restore 2.9 s (a fresh
jail plus the replay), and close 1.1 s.

The run left nothing behind: no jails (live or dying), no zombies, no mounts, no upper
directories. The refusals still held: unsigned, duplicate instance, and root caller.

**Still open for M4:** `DirStore` pointed at a directory on a Tessera volume.

## 9. What this reuses

Almost none of this is new infrastructure; it is composition, which is the point of the
thesis. Tessera CAS (store, dedup, offline, integrity) · Portcullis/`jaild` (per-document
and per-app jails) · Fresco + Pergola (render, chrome) · Limen (composition) · Aqueduct
(control plane, remote UI) · Nomenclator (naming) · Opifex + Sigstore (bundle delivery
and trust) · RCTL/memoryd (resource bounds) · Laminar (scheduling, energy attribution).

## 10. Open questions

1. ~~**Per-document jail launch cost.**~~ **ANSWERED (§4.7c): ~25 ms to open, ~26 ms to
   close, per document; navigation unaffected. The one-jail-per-document default stands,
   and per-site jail reuse is not built.** Reopen if p99 open grows with concurrency.
2. ~~**Web fonts.**~~ **SETTLED in atrium-document-profile-v1.md ("Web fonts — admitted, as
   a required input").** Content-addressed, present before layout, parsed and shaped
   only in the worker, and one canonical form (bare sfnt), produced by the converter's
   decode → subset → sanitize step. This entry was stale; §6's "web fonts are a later
   addition" is superseded by it.
3. **Where HTTP freshness lives.** Content addressing gives integrity and offline but
   not "is this still the current page at this name" — a Nomenclator question, not a
   store question.
4. **Profile v1's exact property set** — to be enumerated in its own document before M2
   starts, since M2's gate is meaningless without it.

## 11. Relationship to other specs

[atrium-navigator.md](atrium-navigator.md) (D6 thesis and decomposition) ·
[insula.md](insula.md) (app model, trial-launch) ·
[portcullis.md](portcullis.md) (jails, capability manifests) ·
tessera-fs.md §20 (dedup domains) · aqueduct.md §6.6 (possession-scoped negotiation) ·
[toolkit-backends.md](toolkit-backends.md) (Servo, the legacy-content tail).
