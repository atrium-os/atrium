# Forum shell in Pergola — implementation scoping

Status: scoping, 2026-08-07.
Source of truth for *what to build*: the design handoff
(`Atrium Shell.dc.html` + README, "Handoff: Atrium Shell (Forum) — desktop UI"),
which instantiates `docs/design/atrium-visual-language.md`.
Source of truth for *how it's allowed to work*: `docs/spec/forum.md` (WM model),
`docs/spec/pergola.md` (toolkit), `docs/spec/atrium-pkg-install-ux.md` (shelf),
`docs/spec/vestibulum.md` (login).

This doc maps the handoff onto the tree as it exists today and phases the work.

---

## 0. Where we actually are

What the handoff needs vs. what exists (audit of `pergola/`, `forum-*/`,
`fresco-*/`, 2026-08-07):

**Already real (don't rebuild):**
- `WM_ENUMERATE` / `WM_DECLARE_LAYOUT` / `WM_SET_RENDERING` — implemented,
  atomic, capability-gated (refusal tested). Render-gating is honored by the
  compositor. F0–F2 done; F3 landed minus the shelf.
- `forum-wm` policy core (1199-line pure-policy lib): roles→layers, work-area,
  split/zoom/snap/stash intents, workspaces, keybindings, `forum-ctl` wire
  (postcard/UDS), app rules. This *is* the state machine the prototype's ~200
  lines of JS logic describe — the concepts map 1:1.
- Tokens: `pergola/src/theme/tokens.rs` is a faithful, near-complete
  transcription of the visual language (ramps, semantic getters, springs,
  radii, sizes). Light/dark `Semantic` struct exists.
- Text: server-side shaping (rustybuzz+swash), IBM Plex bundled, variable-`wght`
  wired end-to-end. `OP_TEXT_MEASURE` exists on the wire.
- Icon flattening: `pergola/src/icon.rs` parses SVG paths and strokes them as
  segment quads (Lucide icons render today).
- Damage-driven present (`OP_WINDOW_PRESENT_DAMAGE`) exists server-side.
- `vestibulum` is a running Pergola login app; `forum-dock`/`-bar`/`-overview`
  are running Pergola chrome apps (crude visuals, real plumbing).

**The gaps, concentrated in Pergola's middle layer:**

| # | Gap | Handoff features blocked |
|---|---|---|
| 1 | Layout = one Stack primitive; every app hardcodes absolute rects | everything |
| 2 | Text measurement = `size * 0.55` estimate | chips, seams, rows, centering |
| 3 | Two widgets (Button, TextField-MVP) | all overlays, lists, popovers |
| 4 | No path *fill* on the Pergola wire (server tessellator unreachable) | Phosphor glyphs, the 7 custom icons |
| 5 | No animation runtime (tokens only) | layout springs, toasts, caret, progress |
| 6 | No hover, no focus ring, no Tab order | every interactive affordance |
| 7 | No `Window`/app-loop API; 5 hand-rolled event loops | maintainability of 6+ chrome apps |
| 8 | No gradient / group-opacity / rounded-clip on the envelope path | wallpaper, tiles, stash dim, surface cards |
| 9 | No notification subsystem | Praeco shelf, toasts |
| 10 | Layouts not persisted as objects (F4) | workspace chip/popover semantics |

---

## 1. Decisions to settle before code (D-gates)

**D1 — Reconcile handoff ↔ visual-language deltas.** The handoff drifts from the
locked doc in places: `bg-elevated` `#FFFFFF/#22282E` vs `neutral-50/neutral-800`;
dock tiles rx-10 (off the shape scale) *with* drop shadows (against the shadow
policy); 13px UI base vs the doc's 15px `md` default; off-grid paddings
(14/6/9px); bar 38px vs toolbar 40px; seam 28px vs titlebar 32px.
Proposal: one §13-process revision of the visual-language doc that (a) blesses a
**dense-shell type tier** (13px UI in shell chrome; apps keep 15px `md`),
(b) adds `radius-tile 10` or moves tiles to `radius-md 8`, (c) either blesses the
tile micro-shadow as the *one* surface exception or drops it (recommend: drop it,
keep the hover translate), (d) snaps off-grid paddings to the grid where the eye
can't tell (14→16 hurts the bar; alternative: bless 14 as a named exception),
(e) records `seam-height 28` and `bar-height 38` as shell dimensions.
Until D1 lands, the token file must not fork from the doc.

**D2 — Who draws the seam.** The handoff's defining element (28px Forum-owned
strip on every surface) has three candidate owners:
- (a) **frescod draws it** (mechanism), content *declared* by forum-wm in an
  extended `WM_DECLARE_LAYOUT` slot payload `{title, role_label, jail_id,
  engine_state}`. Precedent: a complete-but-dead titlebar composer already sits
  in `fresco-scene-server/src/window/mod.rs` (no callers). Seam becomes
  **unforgeable** — composited above app content by the same code that clips it;
  an app cannot draw a fake seam. Forum stays pixel-free per spec §1.
- (b) forum-wm creates one Pergola seam surface per app surface (chrome layer).
  Forum starts pushing pixels; N extra surfaces; placement/atomicity with the
  owning surface needs care.
- (c) a `forum-seam` chrome app — worst of both.
**Recommendation: (a).** The seam is identity + engine-state — that's trusted-UI
territory, same argument as the compositor-owned capability-prompt layer. Revive
the dead decoration path on the envelope pipeline, seam content in the layout
declaration, seam *input* (drag = snap intent, glyph clicks = zoom/stash intents)
routed to forum-wm — which already owns input routing.

**D3 — Icon strategy = Phosphor (per §9) + the 7 custom SVGs.** Both are
*filled* outline paths, not strokable centerlines — `icon.rs`'s stroke-quad
flattening cannot render them. This forces gap #4: expose the server's existing
fill tessellator (holes, cubics — already implemented in
`fresco-scene-server/src/render/tessellate.rs`) via a `PATH_FILL` wire op +
`Node::PathFill`. No new rasterization code — just the op, the node, and
client-side path encoding. Licenses all fine (Phosphor MIT, Plex OFL, taffy MIT).

**D4 — Font weights.** Handoff needs Sans 400/500/600/700 (have: variable, wired)
and Mono 400/500/600 (have: 400/700 statics only; Plex Mono has no variable).
Ship `IBMPlexMono-Medium.ttf` + `-SemiBold.ttf` and extend the resolver table in
`fresco-scene-server/src/text.rs`.

**D5 — Effects budget.** The handoff uses group-opacity (stashed chips 45%,
zoom-dim 35% + `saturate(.55)`) and one popover shadow. Scope: per-node **alpha
multiplier** on rect/text/path ops (cheap, needed everywhere); zoom-dim as a
per-surface translucent **scrim rect** composited by frescod (no saturation
filter — accepting a visible delta from the HTML, or a per-surface
desaturate uniform in the compositor if it's a one-liner in the kernel);
popover shadow as a **9-slice pre-blurred texture** or 3 nested translucent
rounded rects — no blur pipeline. Rounded **clip of composited app surfaces**
(surface-card rx-8) is a compositor feature: per-window corner radius in the
layout declaration.

---

## 2. Workstreams

### A. Wire + frescod (unblocks everything else)
- A1. `PATH_FILL` op → existing tessellator; `Node::PathFill` in Pergola;
  `icon.rs` grows a fill mode. Exit: a Phosphor glyph + `app-navigator.svg`
  render in a demo. (D3)
- A2. 2-stop linear gradient fill on `RectParams` (wallpaper, app tiles,
  progress bar). One subtle gradient per surface is the doc's own cap — the op
  stays minimal.
- A3. Per-node alpha multiplier. (D5)
- A4. Per-window corner radius + seam strip in `WM_DECLARE_LAYOUT`; revive the
  decoration composer for seams per D2(a); seam input → forum-wm.
- A5. Wire `OP_TEXT_MEASURE` into Pergola with a client cache; delete the
  `0.55` estimate.

### B. Pergola core
- B1. `Window`/app-loop API (spec §6): own create → event-translate → tick →
  diff → damage-present. Port vestibulum + the three chrome apps to it (deletes
  4 hand-rolled loops; `display_info()` everywhere, kills vestibulum's
  hardcoded 1280×720).
- B2. Layout: integrate **taffy** (spec §3 named it) behind `Node::Stack`-style
  API — flex row/column, gap, padding, grow, align, absolute-position escape
  hatch. The shell needs real flex (bar spacers, popover rows, launcher list,
  overview grid); building a bespoke half-flex now means rebuilding at D5.
- B3. Interaction: hover tracking (PointerMove → enter/leave per node), pressed
  state, focus ring drawn from `focus_ring` token, Tab order (declaration-order
  traversal). Cursor kinds (default/pointer/grab/col-resize/row-resize) — needs
  a cursor-set op if frescod lacks one (verify; likely a small add).
- B4. `pergola-anim` MVP, client-driven per spec §4.1: spring integrator
  (SNAPPY/GENTLE) + duration tweens driving node properties; `reduced-motion`
  collapse; caret-blink and toast timers. Present path switches to
  damage-present (the diff already knows the rects).
- B5. Widgets, in dependency order, all token-pure (§10 rule):
  `Label` (styled text, auto-measured) → `Icon` (fill) → `Divider` → `Chip` →
  `Badge/Tag` → `Avatar` → `ProgressBar` → `ListRow` → `ScrollView` (vertical
  first) → `TextInput` v2 (caret + blink, Esc/Enter/Home/End; selection later)
  → `Button` v2 (hover/active/focus from tokens) → `Popover` (anchored,
  shadow, dismiss-on-outside) → `Dialog` + `Scrim` → `Toast`. Tooltips deferred.
- B6. Theme runtime: light/dark switch at runtime (re-render with the other
  `Semantic`); add the few missing semantic entries the handoff uses (terminal
  bg/text, scrim, wallpaper stops, grid-line). App identity tints + tile
  gradients are **app metadata** (manifest/catalog), not theme tokens.
  Engine-state → color mapping (deadline=accent, best-effort=info,
  gated=tertiary) lives in a small `forum-theme` helper, not core tokens.

### C. Shell apps (Forum chrome) — each is an exit-criteria milestone
- C1. **forum-bar v2** — brand block, session line, surface chips with
  engine-state dots, centered workspace chip, right icon row, clock. Needs a
  **`forum-ctl` subscription stream** (today: one request/reply per
  connection): `Subscribe → stream of {surfaces, focus, engine states,
  workspace}` deltas. This feed also powers seams, overview, and the HUD —
  build it once, first.
- C2. **Seams live** (A4) — identity + engine state on every surface; drag ≥8px
  → snap-region overlay (forum-wm declares candidate regions; overlay drawn in
  the reserved layer); glyph intents zoom/stash; ratio dividers with
  transition-suppressed drag and atomic commit. Toast narration on commit.
- C3. **forum-dock v2** — 56px rail, custom-icon tiles, running bar, focus
  ring, hover lift; launcher overlay (scrim + search + live filter + status
  tags). Keyboard focus for chrome overlays must route correctly while an app
  holds "app focus" — forum-wm distinguishes overlay-focus from surface-focus.
- C4. **Capability prompt** — Forum-reserved layer, hosted per spec §3 by the
  dock. Requires the portcullisd interactive-grant flow (launch request →
  pending → prompt → allow/deny → jaild). If the daemon flow isn't ready,
  land the UI against a stub intent so the visual + input path is proven.
- C5. **forum-overview v2** — scrim card grid, tile-gradient headers, state
  lines, stashed at 60%, click = summon+focus (intent exists).
- C6. **Praeco + forum-shelf** (new): minimal `praecod` or a praeco lib inside
  forum-wm's session (decide: separate daemon vs forum-owned bus) with
  `Notify`, `Progress{id, phase, fraction, detail}`, `Toast` ops over
  postcard/UDS; shelf app renders install cards (opifex feeds `Progress`) and
  notification cards; toast surface bottom-center. All shell toasts ("snap
  committed — one atomic WM_DECLARE_LAYOUT") route through it.
- C7. **Workspaces as objects (F4)** — forum-wm serializes
  `{slots, stash, focus, ratios}` to Tessera-backed storage keyed by content
  hash; ref names in `forum.toml`-adjacent state; rename=ref-only,
  delete=drop-ref (detached `#····` state), snapshot=new object; switch =
  persist-outgoing → atomic apply; a target needing an unlaunched app gates on
  the C4 prompt. Workspace popover UI on the bar chip.
- C8. **Engine HUD** — toggleable panel fed by the C1 stream;
  per-surface state rows + legend. Real Hz once the frame-pacing loop stamps
  frames.
- C9. **Vestibulum v2** — restyle to the handoff (wallpaper + grid, 340px
  rx-16 panel, avatar, footer strip), `display_info()` sizing, unlock →
  ostiarius handoff toast.

### D. Explicitly out of scope here
The reference *content* (atrium-edit's editor, stoa's terminal text,
atrium-files' list, navigator's document) — the shell must treat surfaces as
opaque. Demo parity uses existing bring-up apps in the slots. atrium-edit /
atrium-term Pergola rewrites remain D5 canaries per ROADMAP.

---

## 3. Milestones (each VM-verifiable, screenshot-diffed against the prototype)

| M | Contents | Exit criterion |
|---|---|---|
| M1 | D1 token revision; §12 reference render updated | reference screen matches handoff tokens, light+dark |
| M2 | A5, B1, B2, B3 partial, B5 through `ListRow` | **forum-bar v2 pixel-matches the handoff** (both themes) |
| M3 | A1–A4, C2, B4 | seams + drag-snap + springs; snap overlay + toast loop works end-to-end in-VM |
| M4 | B5 overlays, C3, C5, C4-UI | launcher, overview, capability prompt render + drive intents |
| M5 | C6, C7, C8 | workspace switch persists/restores through Tessera; shelf shows a live opifex install |
| M6 | C9 | boot → vestibulum → authenticate → shell, all styled |

Sequencing rationale: C1's subscription stream and A1's fill op are the two
items with the most downstream consumers — they go first inside their streams.
M2 is deliberately the bar, not a toy: it exercises measure/flex/hover/chips/
icons/clock without needing any compositor changes, so toolkit work and
frescod work (M3) proceed in parallel.

## 4. Open questions
- Desaturation on zoom-dim: scrim-only, or a per-surface uniform in the
  compositor kernel? (D5)
- Praeco: separate daemon vs forum-session-owned bus? Leans daemon-lite
  (per-session, like the choragus pattern) — decide at C6.
- Cursor-shape op in fresco wire — exists? (verify at B3)
- The `forum-control` capability gate is still peer-uid — the principled
  portcullis-peer gate should land with C4's grant flow (spec §10 note).
- Workspace-popover "roams via stoa" toast: F4 stores locally; actual roaming
  waits on Stoa S3 scrollback/session work — copy stays honest until then.
