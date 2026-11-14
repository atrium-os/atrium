# Resolution and text-size independence

Status: **accepted** (2026-11-14). Shankar agreed with the recommendations in §11.
Covers Pergola, frescod, Forum and the Navigator. No backward compatibility is kept.

## 1. The problem

On most systems, apps size things in device pixels, so one of these happens:

1. **Higher resolution, everything shrinks.** A 13 px label is 13 device pixels on a 1080p
   and on a 4K panel of the same size, so it is half as tall on the 4K one.
2. **Windows scale, text doesn't** (or the reverse). A scale factor reaches some layers and
   not others: the window grows but the app still lays text out in pixels, or the text grows
   and the controls around it stay put, so labels clip and overflow.
3. **Proportions break.** Some sizes scale (fonts), some don't (a 1 px border, a 32 px
   button, a 200 px sidebar), so a dialog designed at 100% looks different at 150%.
4. **Text size changes break layout.** Bigger text wraps differently, and anything sized
   for the old wrapping (fixed-height rows, inline images, "fits on one line" toolbars)
   breaks.
5. **Moving a window between monitors** of different density makes it jump in physical
   size, or it is drawn blurry on one of them.

The goal: **what you see is the same physical size on every monitor and at every
resolution, unless you ask for it bigger; and asking for bigger text makes everything
that holds text grow with it, consistently.**

## 2. What the code does today (atrium c593a81)

Some of the right pieces exist, but they don't agree with each other. Found by reading the
code; none of the per-scale paths has run live at a scale other than 1.0.

| # | Where | What it does | Effect |
|---|---|---|---|
| 1 | Pergola `surface.rs`, `node.rs` | Layout is in logical px; the surface multiplies by `scale` at emission; text is shaped at the device size (never bitmap-scaled). | Right. |
| 2 | frescod `outputs::auto_scale` | Per-monitor scale = diagonal ppi / 110, in 0.25 steps, 1.0 to 3.0. | Right idea; see 7, 8. |
| 3 | fresco-scene-server `update_window_scales` | A window gets `EV_WINDOW_DPI_CHANGED` with the scale of the monitor it mostly covers. | Right. |
| 4 | Pergola `window.rs` | `Resized`'s width/height (device px on the wire) are taken as **logical** px. | At scale 1.5, content is laid out 1.5x too wide and runs off the window. |
| 5 | Pergola `input.rs` | Pointer x/y (device px) are used as logical px, not divided by the scale. | At any scale other than 1, clicks land on the wrong control. |
| 6 | Pergola `Window::open_on` | `SizeSpec::Fixed(w, h)` is passed to `window_create` as device px, and the scale starts at 1.0 until the first DPI event. | Windows open small on HiDPI, then the first frame is drawn at the wrong scale. |
| 7 | frescod / forum-wm | The desktop, monitor rects and window rects are in **device** pixels. A scale change resizes nothing. | A window moved from a 1.5 to a 1.0 monitor becomes 1.5x bigger physically; tiles stay sized for the old scale. |
| 8 | forum-wm `displays.scales` | A scale the user sets is stored as an absolute number per monitor. | Change that monitor's resolution and the override pins the wrong scale: the problem in §1.1 comes back. |
| 9 | `auto_scale` | A monitor with no physical size in its EDID (TVs, projectors) gets 1.0. | A 4K TV gets a tiny UI. |
| 10 | frescod `chrome.rs` | The origin strip is `STRIP_H = 24` device px. | The trusted strip shrinks on HiDPI. |
| 11 | `pergola/src/theme/tokens.rs` | Control heights are fixed numbers (button 32, row 32, toolbar 40), not derived from the type size. | There is no system text size yet; adding one would make text outgrow its controls (§1.2). |
| 12 | `EV_WINDOW_DPI_CHANGED` doc | Says 1.0 = 96 dpi; `auto_scale` uses 110 ppi. | Two references. |

## 3. Units

One unit for everything an app sees: the **point** (pt).

- **1 pt is a fixed angle at the eye**, not a pixel. Reference: 1 pt = 1 px on a 110 ppi
  desktop monitor at arm's length (what `auto_scale` already uses; the 96 dpi comment goes).
- **Device pixels** exist only in two places: inside frescod (scanout, compositing) and at
  Pergola's emission step (the scene the app sends is in its buffer's device pixels, so text
  and paths are rasterized at full resolution and nothing is resampled).
- **Window geometry, the desktop, monitor rects, pointer positions, popup anchors, slot
  rects, min/max sizes**: all in pt, as `f32` (sub-pixel pointer positions survive).
- `scale` = device px per pt, per window, from the monitor it lives on. An app never picks it.

Everything an app or the WM reasons about is in pt; the only code that multiplies by
`scale` is Pergola's emission, frescod's compositor, and renderers of embedded surfaces
(§9). This is the macOS "points and backing scale" model, applied per monitor, and it fixes
rows 4 to 7 of §2 by construction.

### 3.1 Wire changes

Built in step 1a (2026-11-14); fresco-protocol's "Units" note is the reference.

- **Points** on the app connection: `WindowCreatePayload` sizes, pop-up create/place/grant/
  claim sizes and anchors, `EV_WINDOW_RESIZED`, `EV_POPUP_PLACED`, pointer positions, scroll
  deltas, drag and drop positions.
- **The create reply** (`WindowCreateReply`, for windows, pop-ups and claims) carries the
  window's id and the scale it opens at, so the first frame is right (row 6). A new window
  opens at the scale of the monitor at the desktop's origin; a pop-up at its parent's.
- **`EV_WINDOW_DPI_CHANGED` is gone**: `EV_WINDOW_RESIZED` carries `{ width, height }` in pt,
  `scale_milli` and `buffer_width/height` in device px, and is sent on a resize or a scale
  change.
- **Device px** stays for what is drawn: the scene (rects, paths, glyphs, fills, textures,
  damage) and what is placed inside it like content (video layer rects, slot rects). A
  window's buffer is `round(size_pt * scale)` and frescod reports the buffer it chose.
- **`DisplayInfoResponse`** gains `scale_milli` (new windows' scale), so a full-screen app
  asks for `mode / scale` points.
- frescod converts at the client boundary: incoming sizes in the client-facing frontend (a
  split renderer's own), outgoing events where they're encoded for the client, each at that
  window's scale. Inside frescod and between broker and renderers everything stays device
  px; a claim's size reaches frescod in pt, since only frescod knows the bar's scale.
- Clients that lay out in device px themselves (Arca's UI, the lock screen, the player's
  controls, libatrium's C ABI) call `Connection::in_device_px` on each event; moving them to
  Pergola or to points is later work.
- **Step 1b, built 2026-11-14:** the desktop and WM ops (`OP_WM_OUTPUTS`, arrange, move,
  declare layout) stay in device px, because one desktop spans monitors of different scales
  and the WM is a system client that frescod trusts with raw geometry. What forum-wm sizes in
  its own units is stated in pt and converted per monitor: each `Screen` carries the monitor's
  `scale_milli`, and the bar (38 pt) and dock reservations, the dialog size
  (480 x 320 pt) and the PiP minimum and margin (160 x 90 and 16 pt) go through `Screen::px`,
  rounding up. A window's size is the WM's to set, so on a scale change frescod keeps its
  buffer and the window's pt size follows the new scale; a pop-up keeps its pt size. The dock
  redraws on a live scale change, and reports how tall it draws (§7.2). Floating windows keeping their pt size across monitors is
  step 6.

## 4. Per-monitor scale

### 4.1 Automatic scale

- From EDID size: `ppi / 110`, rounded to **1/8** steps (0.125) rather than 0.25, clamped
  1.0 to 4.0. Finer steps keep physical sizes within about 6% across common panels.
- **No physical size in EDID** (TVs, projectors): assume a living-room or wall distance and
  scale by mode height, `max(1, height / 1080)`, so a 4K TV gets 2.0, not 1.0.
- **Laptop panels** (eDP connectors) are seen closer; open question, §11.

### 4.2 The user's size preference

The user doesn't set a scale; they set **"UI size"** per monitor as a multiplier on the
automatic scale: Smaller (0.875), Default (1.0), Larger (1.125, 1.25, 1.5, 2.0).

`scale = auto_scale(monitor, mode) x ui_size(monitor)`

Because the preference is relative, changing the resolution keeps the physical size (row 8):
a 27" monitor at 4K is 1.5 x Default, at 1440p it is 1.0 x Default, and windows look the
same size, only sharper or softer. forum-wm stores `ui_size` per monitor identity instead of
the absolute scale; `forum-ctl scale` becomes `forum-ctl ui-size`.

**Built in step 2a (2026-11-14).** `fresco_protocol::auto_scale` and `output_scale` hold the
rules above; frescod reports each monitor's `auto_scale_milli`, `ui_size_milli` (with
`ui_size_set`) and `internal` (an eDP connector) in `WmOutput`, and `scale_milli` is their
product. `OP_WM_SET_OUTPUT_UI_SIZE` (0x0527, was `OP_WM_SET_OUTPUT_SCALE`) takes one of
`UI_SIZES` or 0 for the monitor's default; anything else is refused. forum-wm remembers UI
sizes per monitor (`uisize` lines in forum-displays.state; old absolute `scale` lines are
dropped) and `forum-ctl ui-size N smaller|default|larger|1.25` sets them.

### 4.3 Resolution change on one monitor

Mode change -> new auto scale -> the monitor's pt size stays (about) the same -> forum-wm's
layout in pt doesn't change -> each window gets `EV_WINDOW_SCALE` and redraws at the new
density, same size, same wrapping. Nothing re-tiles, nothing shrinks.

## 5. Fractional scales and crispness

At 1.25, 1.375 or 1.5, a 1 pt line is 1.25 to 1.5 device px: blurry if drawn as is. Pergola
snaps at emission, not in layout:

- **Edges** of rects, borders, focus rings and dividers snap to device pixel boundaries.
  Layout keeps exact pt values; only the drawn edge is snapped, so proportions don't drift.
- **Hairlines and 1 pt strokes** are `max(1, round(scale))` device px wide (visual language:
  "1px logical, crisp at any DPI").
- **Text baselines** snap to device px vertically; glyphs are rasterized at the device size
  (as now). Horizontal glyph positions stay fractional.
- **Icons** are vector (Phosphor), flattened at the device size (as now in `icon.rs`).
- frescod never resamples a window's buffer except in §7's straddle case and in mirroring.

**Built in step 3 (2026-11-14)**, all in `FrescoSurface` (pergola/src/surface.rs), so no
widget changes: `snap_span` rounds a rect's start and end to device pixels, and a span of
2 pt or less (a hairline, a border, a divider) is `max(1, round(w x scale))` pixels wherever
it lands; group and scroll origins, scroll clips and backdrops are whole pixels, so what a
group holds keeps its snapped edges; horizontal and vertical stroked paths are a whole number
of pixels wide with their edges on pixel boundaries; images snap like rects;
`text::wire_top_device` puts each text run's baseline on a device pixel (x stays
fractional). Radii, diagonal strokes and filled paths are not snapped. Edges move by at most
half a pixel and sizes by at most one, at 1.0 too, where layout gives fractional positions
(centred content).

Live check (Mac VM, 2026-11-14): at 1.25 every 1 pt border is a solid 1 px. At 1.5 a
bordered box's far edge came out 1 px where the near edge was 2, because a border is an
outer rect plus a body inset by 1 pt and each edge was snapped on its own; no rounding of
single edges can make every 1 pt step 2 px at 1.5. Fixed (2026-11-14): `FrescoSurface` keeps,
per group, where each drawn edge went, and places an edge 2 pt or less from one already
placed from that one (`max(1, round(d x k))` pixels away), preferring the edge outside the
span; a group's own box (0 and its size) counts as placed edges for what it holds. So a
border inset, a child group's inset and a divider at a box's edge are equally wide on all
sides at every scale, and neighbours that share an edge still meet. Unrelated edges round
as before. Not yet re-checked live.

## 6. Text size and relative sizing of widgets

This is the core of Shankar's second point: widgets must keep their proportions, and must
grow with text when text grows.

### 6.1 Two scales, and what each one moves

| Knob | Who sets it | What grows | What stays |
|---|---|---|---|
| **Display scale** (§4) | Per monitor, automatic x UI size | Everything, uniformly: text, controls, gaps, icons, images, window sizes. Proportions are identical at every scale. | Nothing changes shape. Wrapping is identical. |
| **Text size** (new, accessibility) | One system setting: 85%, 100%, 115%, 130%, 150%, 175%, 200% | Text, and everything sized from text: control heights and paddings, row heights, inline icons, text-relative gaps, min window sizes. | Layout gutters between groups, window margins, hairlines, photos at their own size. |

A third, **content zoom** (Navigator, documents, image viewers), is per app and scales only
the content area, not the chrome (§10).

Display scale never changes wrapping. Text size does, on purpose, and the layout has to
absorb that (§6.3).

### 6.2 Sizes come from text, not from fixed numbers

In Pergola, every size token becomes one of three kinds, and a widget may only use these:

- **`em`-based** (text-relative): control heights, inner paddings, row heights, inline icon
  sizes, the gap between an icon and its label. Defined from the control's own type size and
  line height. `BUTTON_HEIGHT_DEFAULT = 32` becomes "line height of md text + 2 x 0.55 em
  padding", which is 32 pt at 100% text and grows with it.
- **`pt`-based** (layout): gaps between groups, window and panel margins, sidebar minimum
  widths, the spacing scale between sibling blocks. They follow the display scale only.
- **Hairline**: 1 pt borders and dividers (§5).

This is what keeps relative sizing right: at any display scale everything multiplies by the
same number, and at any text size a control and its label grow together, so a label never
outgrows its button and a row never clips its text. The touch target rule (1.25x) becomes a
floor in pt (44 pt on touch, 24 pt with a pointer) applied after the em size.

A lint (test) walks the widget catalogue at text sizes 85% to 200% and display scales 1.0 to
3.0 and fails on clipped text, overlapping controls, or a control below the touch floor.

### 6.3 When text gets bigger, layout reflows

- **Wrapping**: Pergola already wraps text leaves to the width they get; bigger text gives
  more lines and taller rows. Rows are never fixed-height unless the widget is a virtual list,
  and virtual lists measure their row height from the text size (re-measured on change).
- **Toolbars and button rows** that no longer fit move items into an overflow menu (lowest
  priority first) rather than clipping or shrinking text. Labels may drop to icon-only only
  where the widget declares an icon.
- **Min window size** is the layout's natural minimum, sent to the WM as a hint. When text
  grows, min sizes grow, and forum-wm re-tiles if a window no longer fits its tile.
- **Scroll position** is kept by anchor (the first visible element and the offset into it),
  not by pixel offset, so a reflow doesn't jump the reader. ScrollView already anchors zoom
  settles this way; the same anchor is used for text-size and width changes.
- **Truncation** with an ellipsis is a last resort, only where the widget says so, with the
  full text in a tooltip and in the accessibility tree.

### 6.5 Built so far (step 4, 2026-11-14)

- **4a, the setting:** `AppearancePayload.text_size_milli` (one of `TEXT_SIZES`, 850 to
  2000; 0 = 100%), validated by frescod, carried to split renderers and broadcast in
  `EV_APPEARANCE_CHANGED`. `forum-ctl text-size 130` sets it through forum-wm, which keeps the
  scheme, remembers it (`textsize` in forum-displays.state) and restores it at start.
- **4b, Pergola:** one text size per process (`text::text_scale`, set from the appearance;
  a change relays out). `TextStyle.size` stays the type ramp's pt and `TextStyle::em()` is
  what is measured, wrapped, shaped and drawn. Control heights are em-based through
  `theme::size::{button_height, input_height, menu_item, list_row, toolbar, ...}`; the
  constants are the 100% reference. A catalogue lint (pergola/tests/text_size.rs) lays out
  the controls at every text size and checks each label fits its body.
- **4c, the bar:** `shell::bar_h()` is 38 pt times the text size, rounded; forum-bar tells
  forum-wm with `Intent::ReserveTop { pt }` at start and on each change, and forum-wm gives
  the bar that height (24 to 96 pt), remembers it (`bar` in forum-displays.state) and keeps
  it across mode changes and restarts.
- **More em-based sizes (2026-11-14):** button and menu paddings, the menu's widest,
  checkbox, radio and toggle parts and the check mark, the slider, the bar's volume panel and
  Scrinium's rows, bars and sidebars. Document content (Officina's grid, a page) keeps its own
  sizes and follows its zoom, not the text size.
- **Anchor-kept scroll:** before each render `App` notes, per scroll view, the paragraph or
  label nearest the top and how far into it the top is; after layout, if the view's position
  did not change but that text moved (a text size or width change), the position moves with
  it and the view's state is told (`Ctx::on_reflow`). A wrapped paragraph is matched by its
  words, so it is found again however it rewraps; a reader's own scroll is never undone.
- **Minimum window size:** Pergola measures the natural minimum of the stack the app names
  with `Ctx::min_size_from` (a flex root filling the window; a hand-placed layout must not,
  as its minimum would be its current size, and without it no minimum is sent) (taffy at
  min-content width; the content's height at the window's width) at open and whenever the
  text size or the width changes, and sends it in points with `OP_WINDOW_SET_HINTS`
  (`WindowHints.min_size`, never below the app's own; at most `MAX_MIN_SIZE_PT`, 8192).
  frescod keeps it per window (split renderers pass it as `WINDOW_MIN_SIZE`), reports it to
  the WM in device pixels at the window's scale (`WmSurfaceInfo.min_size`) and tells it with
  `EV_WINDOW_MIN_SIZE` (WM only). forum-wm gives split columns at least their documents'
  minimums (stacking them when the minimums don't fit side by side), grows a dialog about its
  centre and a panel leftwards, and gives a snapped document too wide for its half the work
  area; nothing leaves the work area. Live check (2026-11-14): the chain works end to end
  (a split column got exactly its document's minimum; two that didn't fit stacked); borders
  are 2 px on all four sides at 1.5 and 1 px at 1.25 after keeping glyph strokes and dots out
  of the edge table (5835fad9). A scroll view's natural minimum height is 0.
- **Toolbar overflow:** `pergola::widgets::Toolbar` lays out `ToolItem`s (label, priority,
  enabled, action) in its width; when they don't fit, the lowest priorities (the later of
  equals first) move into a "More" menu at its end, in their order, and come back when there
  is room. Scrinium's actions row uses it. Icon-only items: `ToolItem::icon(ch)` / `Button::icon(ch)` draw the icon in a square button (label as tooltip, and as the entry in More).
- **Stated minimums:** an app laid out by hand states its minimum in em with `Ctx::min_size(Size)` (preferred over `min_size_from`); Scrinium's manager does (sidebar + 360 em-pt by 360).
- **App-shaped `Glyphs` text is document content**, not UI: a document view
  (`pergola-document`) and Officina's `pergola-richtext` follow their own zoom (whose default
  can start from the text size), as §6.1 says for content. So `Node::Glyphs` does not follow
  `text_scale`; nothing to build.

### 6.4 Inline images and other inline objects

An inline object in a line of text (an image, an icon, an emoji, a chip, an embedded control)
declares how it sizes, and that decides what happens when text or display scale changes:

| Kind | Sized by | Examples | Text grows | Display scale grows |
|---|---|---|---|---|
| **Glyph-like** | `em` (a multiple of the line's font size), sits on the baseline | Icons in labels, emoji, status dots, inline math, file-type badges, mentions as chips | Grows with the text | Grows |
| **Intrinsic** | Its own size in pt (pixels / image density), capped to the line's width | Photos and screenshots in a document or a message, avatars with a set size | Stays; the line gets taller to hold it; wraps to its own line if wider than the remaining space | Grows (it's in pt) |
| **Container-relative** | A fraction of the available width, keeping aspect ratio | Hero images, full-width figures, video | Stays relative to the column; reflows with it | Grows with the column |

Rules that make this predictable:

- **An image's pt size is its pixel size divided by its density**, where density defaults to
  1 (1 image px = 1 pt) and comes from the source when it says (`srcset` 2x, a PNG pHYs
  chunk, an app-declared asset density). An app-shipped bitmap that wants to stay sharp
  ships at 2x and declares it; vector assets are preferred.
- **Decode and upload at device resolution**: the image is decoded or chosen (srcset) for
  `pt size x scale x zoom`, so it's sharp at 2.0 and not wastefully large at 1.0. A change of
  scale re-picks the candidate after the change settles.
- **Line height**: a line's height is the max of its text line height and its inline
  objects' heights; glyph-like objects are sized so they don't increase it (at most 1 em,
  centred on the x-height or on the baseline as the object declares).
- **Aspect ratio is always kept**; an intrinsic image wider than its line shrinks to fit,
  never crops.
- **Placeholders** have the right size before the image arrives (from declared intrinsic
  sizes, which the Navigator's normalizer already requires), so nothing jumps when it loads.

Pergola gains an inline-object leaf in its text flow (`Inline { size: Em | Intrinsic |
Fraction, baseline }`) used by both widgets and DocumentView.

**Built (step 5, 2026-11-14):**

- `pergola::flow`: a `Flow` of `Piece::Text(text, style)` and `Piece::Inline(Inline)`, broken
  into lines together (greedy, at spaces and `\n`; a token wider than a line gets a line of
  its own). `Inline { size, align, content }`: `InlineSize::Em(w, h)` (h at most 1 em, so the
  line keeps its height), `Intrinsic(pt size)` (shrinks to the line, aspect kept, the line
  grows to hold it), `Fraction(f, aspect)`; `InlineAlign::Baseline` (bottom on the baseline)
  or `Middle` (centred on the text's middle, 0.35 em above the baseline); content an image, a
  glyph in a family (an icon, an emoji) or a filled rounded box (a dot, a chip's body).
  Constructors `Inline::image` (own size), `Inline::figure(image, f)` and `Inline::glyph`.
- `Ctx::flow(Flow)` adds one to a layout like `Ctx::paragraph`: it takes the width it is
  given and is as tall as its lines; after layout it becomes a free stack of text runs and
  objects (`Node::Image`, `Node::Rect`, a glyph `Node::Text`), with its first baseline for
  baseline alignment.
- `Image` has a `density` (pixels per point, default 1; `with_density`) and the same picture
  at other densities (`with_alternates`). `pt_size()` is pixels over density. Where it is
  drawn, the window uploads `pick_for(rect, scale)`: the least dense candidate that fills the
  rect pixel for pixel at the window's scale, else the densest. Drawn larger than its own size
  (a wide figure, a zoomed document image) asks for denser pixels too. DocumentView images go
  through the same path, so a document image source that offers alternates (step 7, `srcset`)
  needs nothing more in Pergola.
- Not yet: placeholders before an image arrives (the app gives the `Inline` its declared size
  with a `Box` until then), and re-picking when only the scale changes is the window's full
  resend on a scale change (step 6 checks it live).

## 7. Moving windows between monitors

- A window has **one scale at a time**: that of the monitor it mostly covers (as now).
- **Its size is in pt**, so moving it to a monitor with a different scale keeps its physical
  size and its layout; only its buffer is re-made at the new density.
- **While being dragged**, the window keeps its scale; frescod draws the part on the other
  monitor resampled by the ratio of the two scales, so it looks the same physical size on
  both (slightly soft on the denser one for that moment). On release, if the majority moved,
  the window gets `EV_WINDOW_SCALE` and redraws sharp. No relayout storms mid-drag.
- **A window that straddles for good** stays at its majority monitor's scale, with the
  minority part resampled.
- **Rescue and restore** (multi-monitor phase 4) work in pt, so a window rescued to a lower
  density monitor keeps its physical size, clamped to fit.
- Popups take their parent window's scale, even when they hang over another monitor.

**Built (step 6, 2026-11-14).** Forum places every window itself (a document fills its
monitor's work area or a column of it; a dialog, panel or PiP is sized in points), and it
has no free window drag: a window changes monitor by "move to the next monitor"
(`forum-ctl move-monitor`), by a monitor leaving (rescue) or coming back, or by a span. So
what this section needs today is that those moves keep points:

- frescod already gave each window its majority monitor's scale on every layout, and
  pop-ups and slots their parent's. A layout that moves a window to a monitor of another
  scale now sets its size and its scale under one lock (`update_window_scales_in`), so the
  client never reads the new pixel size at the old scale (a size in points never meant,
  and a layout pass for it). Test: `a_window_moved_to_a_denser_monitor_keeps_its_size_in_points`.
- `WmSurfaceInfo.min_size` is in points now; forum-wm converts it at the scale of the
  monitor it lays the window out on (`min_px`). It was device pixels at the window's old
  scale, so a window moved from 1x to 2x got half its minimum. Test:
  `a_window_keeps_its_points_on_a_monitor_of_another_scale` (a dialog 600 x 400 pt is 1200 x
  800 px on the 2x monitor and 600 x 400 again when rescued to the 1x one).
- Pergola already re-sends everything on a scale change (`App::resend_all`): text is shaped
  at the new size and images re-picked for the new density (§6.4).
- `scripts/vm-multimonitor-chaos.sh` gains `MIXED=1` (default): UI-size changes per
  monitor and moves of the focused window to the next one among the plugs, swaps, mirrors
  and arrangements.
- Not built: drag-time resampling waits for a free window drag in Forum; a span over
  monitors of different scales draws at its majority scale without resampling the rest.

### 7.1 Dialogs, pop-ups and sheets

Everything that hangs from a window is placed relative to it, in pt, and drawn at the
parent's scale, so it keeps its size and its place against the parent on any monitor.

- **In-window dialogs** (Pergola `Dialog`, a scrim over the window): part of the window's
  own layout, so they scale with it. Their size comes from their content (em-based, §6.2),
  capped to the window less a pt margin; past that the body scrolls, the buttons stay.
- **Pop-ups** (menus, dropdowns, the bar's panels; fresco-popups.md): anchor, size and the
  flip/shift at monitor edges are in pt, worked out against the parent's monitor in pt. A
  pop-up takes its parent's scale even when it hangs over another monitor.
- **Sheets** (the Scrinium picker and other Limen `sheet` slots; limen.md §5.1): today
  the catalogue size (picker 640 x 440) and `SHEET_TOP` (48) are device px, so a sheet
  would shrink on a HiDPI monitor and sit too close to the top. They become pt. The sheet's
  process is told the parent's scale like any embedded surface (§9) and renders at it.
  Its size is the larger of the catalogue size and the sheet's own natural minimum (so at
  text size 150% a picker grows rather than clipping), capped to the parent's monitor in pt.
  It stays centred across the parent and hangs past a smaller parent, as now.
- **Modality is unchanged**: dimming, input routing and the focus-ring frame follow the
  sheet's rect, which frescod converts with the parent's scale. The frame and its
  `system:` tag are frescod chrome and scale per §8.
- **Moving the parent to another monitor** moves its pop-ups and sheets with it; they switch
  scale with the parent on release (§7), in one frame, never one before the other.

### 7.2 The bar, the dock, their icons and panels

- **The bar** (forum-bar) is a Pergola app, so its height (`shell::BAR_H`, 38), its text and
  its status icons are in pt and draw at the monitor's scale. Its icons are vector (Phosphor,
  and apps' vector status icons), flattened at the device size, so they stay crisp at 1.25
  or 2.0 with no 1x/2x assets.
- **The space it reserves** is in pt (step 1b): forum-wm's `BAR_H = 38`, the same constant
  Pergola uses, times the monitor's scale, so the bar and the work area always meet exactly.
  Before 1b it was 38 device px, and at scale 1.5 the bar drew 57 px tall over windows laid
  out 38 px down. With text size above
  100% the bar's height is em-based (§6.2) and the WM reserves the bar's reported height.
- **A bar on each monitor** (where Forum shows one) draws at that monitor's scale, so on a
  mixed setup each bar is the same physical height.
- **Its drop-down panels**: the bar's own menus and the volume/clipboard panels are its
  pop-ups (§7.1). Panels an app draws through the bar's grant (forum-status-items.md §4) are
  sized in pt by the app and placed by frescod against the bar's icon at the bar's scale; the
  app's process is told that scale (it opens at it, `WindowCreateReply`). frescod's label gap
  between bar and panel (`HOSTED_LABEL_GAP`) becomes pt too.
- **The dock** (forum-dock) is a Pergola app too, started with the session: its tiles
  (`TILE`, 60) and its Lucide icons are vector and in pt, so they follow the scale like the
  bar. It reports how tall it draws from the bottom of the screen (panel plus margin,
  120 pt today) to forum-wm with `Intent::ReserveBottom { pt }` on the forum-ctl socket,
  which only session components reach; forum-wm keeps that many pt free at the bottom of
  the main monitor at its scale, keeps it across mode changes, clamps it to 240 pt, and
  reserves nothing while no dock has reported (built 2026-11-14; before, forum-wm reserved
  a fixed 48 and full-size windows covered most of the dock). It has no panels yet; when it gets them (a tile's
  menu, a stack of a folder), they are its pop-ups (§7.1). App icons that arrive as bitmaps
  (an app's manifest icon) follow §6.4: declared density, chosen for the device size.
- **The trusted strip** above everything is frescod's (§8), 24 pt.

## 8. frescod's own drawing

The origin strip, the cursor, Arca's ring, the lock screen, focus outlines drawn by frescod
and frescod-side text all use the scale of the monitor they're drawn on: `STRIP_H` becomes
24 pt. The cursor is vector or provided at 1x/2x/3x and picked per monitor.

**Built in step 2b (2026-11-14).** The strip is `STRIP_PT = 24` and frescod's chrome (the
strip, its origin text, the focus ring, the labels over bar-hosted panels, island, sheet and
credential-slot frames) is drawn through `chrome::Px` at the scale of the monitor at the
desktop's origin. The chrome's font is rasterized at build time for quarter steps from 1.0
to 4.0, and the largest table not above the scale is used, so text is never a 14 px bitmap
blown up. `reserved_top` (the strip's rows, sent in `OP_DISPLAY_INFO` and
`EV_DISPLAY_CHANGED`) and the pop-up ceiling follow the scale, and the gap left for the label
over a bar-hosted panel (`HOSTED_LABEL_GAP`) is pt at the bar's scale. Input is in pt at the
source: relative pointer motion moves the cursor `dx x scale` device px of the monitor under
it, a wheel detent scrolls 8 pt and touchpad scrolling is finger travel in pt, so scroll
amounts reach apps unconverted. frescod draws no cursor image today (the pointer is the
hardware's or the host's); when it does, it follows this rule.

## 9. Embedded and non-Pergola surfaces

- **Limen slots** (embedded jailed surfaces, e.g. Navigator pages): the parent places the slot
  in pt; frescod tells the child its scale and buffer size the same way as a window
  (`EV_WINDOW_SCALE`). The child renders at that density.
- **GPU apps / video**: get the buffer size in device px and the scale; video is a texture
  frescod scales anyway.
- **Mirroring** keeps resampling as now.

## 10. The Navigator and its reading controls

Three factors multiply for page content:

`device px = CSS px x zoom x display scale`, and `font size = CSS font size x text size`

- **CSS px = 1 pt at zoom 1.** A page looks the same physical size on every monitor.
- **Display scale** is invisible to the page (no script reads it); it only decides render
  density and which `srcset` candidate is fetched.
- **Zoom** (Ctrl +/-, pinch) scales the content group, layout included (pergola.md §10.4).
- **Text size** in the Aa controls multiplies with the **system text size**: page font
  sizes = CSS size x system text size x per-site text size. A user who chose 130% system-wide
  gets 130% pages by default, and a per-site step adjusts from there. `text_scale_64` gets
  the product.
- **Comfortable** width stays `680 pt x text size x zoom` (navigator-ui.md §7), so it keeps
  about 70 characters per line at any text size.
- **Inline images** in pages follow §6.4: intrinsic from declared sizes, capped to the
  column, re-picked by density. The Navigator's chrome is Pergola and follows §6 like any app.

**Built (step 7, 2026-11-14):** navigator-page lays pages out at the reader's per-site text
size times the system text size (`text_64()`), for layout, the comfortable measure, find and
in-page anchors, and lays the page out again when the system text size changes. The reader keeps their
place: before a reflow (text size or measure) the first word at the top of the view and
which of its matches it is are found (`find::word_at` + `find`), and after it the view is
scrolled so that word sits where it did (the same document px showed text from further
up; found in the live check). Page images: profile §3.11 admits no fetch that varies with the reader's screen,
so the converter picks an `<img>`'s `srcset` candidate for one fixed density, 2x, for every
reader (`navigator-prerender/src/srcset.rs`): x descriptors directly, w descriptors over
`sizes` at the 1280 px conversion viewport (px/vw/em lengths, min/max-width conditions; else
100vw); the least dense at least 2x, else the densest. The candidate is fetched (and hinted)
in place of `src`, kept under the `src` the document names it by, and its natural size is
its pixels over its density, so it takes the room its 1x `src` would and a 2x screen gets
2x pixels. Nothing about the reader leaks; the cost is larger image downloads, bounded by
the existing image ceilings. (Chosen by Shankar 2026-11-14.) A `<picture>`'s first `<source>` that applies at the conversion viewport (media holds, type decodable) is picked the same way.

## 11. Decisions (accepted 2026-11-14)

Shankar took the recommendation on each:

1. **Reference density**: one reference, 110 ppi = 1.0; laptop panels (eDP) are detected by
   connector type and start at UI size Smaller.
2. **Steps**: 1/8 for the automatic scale; the UI size list of §4.2.
3. **Text size range**: 85% to 200%; beyond that is a magnifier's job.
4. **Straddling windows**: resampled on the minority monitor (§7), not rendered twice.
5. **Per-app text size**: none; only the system setting and each app's content zoom.

## 12. Build order (after review)

1. **Units fix** (rows 4 to 7, 12). **1a, built 2026-11-14:** pt on the app connection for
   geometry and input (window, pop-up and sheet sizes and anchors, `SHEET_TOP`), scale in the
   create reply, scale changes as `EV_WINDOW_RESIZED`, Pergola in pt with one place that
   multiplies. **1b, built 2026-11-14:** forum-wm's bar, dock, dialog and PiP sizes in pt,
   converted at each monitor's scale; the desktop itself stays device px. Test: the Pergola showcase
   pixel-compared at 1.0, 1.5 and 2.0 (2.0 must equal 1.0 upscaled 2x in geometry), clicks hit
   at every scale.
2. **Scale policy** (rows 8 to 10). **2a, built 2026-11-14:** `ui_size` relative
   preference, TV fallback, 1/8 steps, laptop panels at Smaller (§4.2). **2b, built 2026-11-14:**
   frescod's strip, chrome and input amounts in pt (§8). Test: change a monitor's mode in the VM and check every window's
   pt size is unchanged.
3. **Crispness** (§5): emission snapping (built 2026-11-14). Test: hairlines are solid device pixels at 1.25,
   1.5 and 1.75.
4. **Text size** (§6.1, 6.2, 6.3): system setting broadcast like appearance, em-based tokens,
   overflow menus, min-size hints, anchor-kept scroll. Test: the catalogue lint.
5. **Inline objects** (§6.4) in Pergola's text flow and DocumentView, with density-aware
   image picking. **Built 2026-11-14** (§6.4 "Built").
6. **Monitor moves** (§7): drag-time resampling and scale switch on release; chaos test
   extended with mixed-scale monitors. **Built 2026-11-14** for the moves Forum has (§7
   "Built"); drag-time resampling waits for a free window drag.
7. **Navigator** (§10): system text size into `text_scale_64`, density-picked images.
   **Built 2026-11-14** (§10 "Built").
