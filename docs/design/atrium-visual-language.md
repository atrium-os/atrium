# Atrium visual language

> *The aesthetic foundation that every Pergola widget, every Atrium app,
> and every system surface inherits.*
> Locked: 2026-05-09 · Owner: Pergola track · Reviewed before phase 0 of
> toolkit implementation. · Rev. 2 (controls, contrast, appearance)
> accepted 2026-11-06, see the revision log.

This is a decisive document, not a survey. Each section commits to a
choice. We can revise; we don't equivocate.

---

## §1. Positioning

Atrium's visual identity in one paragraph:

> **Calm, confident, slightly sharp.** Restrained like Linear,
> typography-forward like Vercel, *honest* like the BSD/terminal
> heritage we ship from. Vector-native — designed for live-rendered
> curves and shape-space animation, not raster constraints. Romanesque
> in spirit (the name is Atrium): stone, daylight, and architecture, not
> chrome and gradients.

What this rejects:

- ❌ Skeuomorphic shadows that exist only because bitmaps can't gradient cleanly
- ❌ Material's saturated palette and aggressive elevation
- ❌ Apple's over-rounded "marshmallow" radii
- ❌ Windows 11's acrylic blur dependency
- ❌ Cool "tech blue" as primary; this is overdone

What this embraces:

- ✅ Type does most of the visual work (size, weight, spacing — not color)
- ✅ Mostly grayscale; color reserved for *meaning*, not decoration
- ✅ Geometric clarity at any DPI (because we render vector)
- ✅ Confident curves — present where they help, absent where they don't
- ✅ Monospace is honored, not hidden, where the content wants it

---

## §2. Typography

**One family for UI, one for code.** Both OFL, permissive, variable.

| Role | Family | Source | Why |
|---|---|---|---|
| UI sans | **IBM Plex Sans** | OFL, IBM | Slightly humanist, slightly engineered. Reads honest. Variable across 100–700. |
| Mono | **IBM Plex Mono** | OFL, IBM | Designed as a sibling to Plex Sans — pairs natively. Looks at home in terminal *and* in code editors. |

We commit to **one type family pair across the entire OS.** No
mixing, no fallback faces, no per-app deviation in the system shell.
Apps may bring their own type, but Atrium-shipped surfaces use Plex.

### Type scale (1.25× modular, 13px base)

```
2xs   10px   machine-text captions (Mono only — hashes, column headers)
xs    11px   captions, secondary metadata
sm    13px   body text, dense data
md    15px   UI default — buttons, fields, menu items
lg    18px   section leads, dialog body
xl    22px   subsection headings (h4)
2xl   28px   panel headings (h3)
3xl   36px   screen headings (h2)
4xl   48px   hero / login heading (h1)
```

**Dense-shell tier** *(rev. 1)*: system-shell chrome (Forum bar, dock,
seams, popovers, launcher, HUD) uses **sm 13px as its UI default**
instead of md — shell chrome is glanced at, not read, and the density
is the point. Apps keep md 15px. Additionally, *machine-text in Mono*
(jail ids, hashes, engine states, terminal content) may use optical
half-steps between 2xs and sm (10.5, 11.5, 12.5) — Plex Mono runs
optically larger than Sans at equal size, and these are its optical
equivalents of the Sans steps. Half-steps are Mono-only, shell-only;
Sans always sits exactly on the scale.

Body line-height: **1.45** for body, **1.25** for headings, **1.0** for
single-line UI text.

### Weight scale

```
regular   400   body, default
medium    500   UI text, labels, button text
semibold  600   emphasis, headings up through xl
bold      700   2xl and larger only
```

We do *not* use weights below 400 (light weights look anemic in dense
UI) or above 700 (black weights belong in display contexts only).

### Letter spacing

Tight at large sizes (-0.02em at 28px+), neutral at body, slightly
loose at xs caption (+0.01em). Single rule per scale step, not
case-by-case.

---

## §3. Spacing — 8pt grid

Single grid: **8px primary, 4px sub-grid for fine adjustments.**

```
xxs    4px   icon-to-text gaps, divider thicknesses
xs     8px   inline element spacing, button padding (vertical)
sm    12px   element-to-element within a group, button padding (horizontal)
md    16px   group spacing, panel padding
lg    24px   section spacing, dialog padding
xl    32px   between major regions
2xl   48px   page margins on large surfaces
3xl   64px   hero spacing (login screen, splash)
```

Rule: **never use a value not on this scale.** If the design wants 14px,
you used the wrong type size; if it wants 20px, pick 16 or 24. This
constraint is what makes the whole system feel coherent.

**Scope clarification** *(rev. 1)*: the scale governs spacing **between
elements** and container padding. A control's *internal* metrics — a
chip's 9px side padding, the 6px gap between a glyph and its
engine-state dot, a 5px status dot — are part of that control's design,
like a glyph's sidebearings, and are free to be optical. The test:
if two sibling elements are separated by it, it's on the scale; if
it's inside one control's border, the control's designer owns it.

---

## §4. Color

A grayscale-dominant palette. Color carries meaning, not decoration.

### Neutrals (12-step ramp, slightly cool)

A cool slate ramp — perceptually linear in lightness. Slight blue
undertone (~210° hue) so neutrals feel like stone, not warm beige.

```
neutral-0     #FFFFFF    elevated surfaces on light (one step above 50)
neutral-50    #FAFBFC    page background, light
neutral-100   #F2F4F6    surface raised by 1
neutral-200   #E4E8EC    subtle dividers
neutral-300   #CFD5DA    enabled-state borders
neutral-400   #A8B0B8    placeholder text, disabled controls
neutral-500   #7C858E    secondary text
neutral-600   #5A636C    tertiary text
neutral-700   #3F484F    body text on light surfaces
neutral-800   #2A3137    headings on light surfaces
neutral-850   #22282E    elevated surfaces on dark (rev. 1)
neutral-900   #181C20    primary text on light surfaces (rarely full black)
neutral-925   #12161A    canvas on dark (rev. 1)
neutral-950   #0E1114    extreme contrast — rarely used
```

*(rev. 1)* Steps 0, 850, and 925 were added when the shell design was
rendered at fidelity: on light, elevation needs one step *above* the
page background (50-on-50 was an invisible elevation — found as a live
bug in Pergola first-light); on dark, the gap between 900 and 950 was
too coarse to express canvas-vs-surface-vs-elevated as three distinct
tones. The ramp stays perceptually ordered; these fill real gaps.

Dark theme inverts this ramp; same step structure, same semantic
tokens, different background end.

### Accent (one — Atrium amber-bronze)

A single warm accent. Distinctive against the cool neutrals. Romanesque
terracotta-bronze. Carries focus, primary action, brand presence.

```
accent-50     #FBF1E5
accent-100    #F4DEC0
accent-200    #E8BE8C
accent-300    #D69E5C
accent-400    #BD7F3A    accent default — buttons, focus rings, links
accent-500    #9E6628    pressed state
accent-600    #7B4F1E    high contrast on light bg
accent-700    #4A3212    accent-bg on dark (rev. 1)
accent-800    #2A2013    accent tint at lowest dark prominence (rev. 1)
```

We commit to **one accent.** Multi-accent OSes (Material's primary +
secondary + tertiary) produce visual noise. Atrium has neutrals + one
warm — that's the entire color story for the system shell.

### Status (semantic only — used where meaning, not aesthetics, requires it)

```
success-500   #2E8B57    sea-green, slightly muted
warning-500   #C99030    deep mustard
danger-500    #B23A3A    iron-oxide red, not crayon red
info-500      #4A7BAB    deep slate blue (when system info is needed)
```

Each has the full ramp (50–950) but shells should default to the 500
step. Brighter steps are for filled badges; darker for text on light.

*(rev. 2)* The text steps, fixed so `-fg` tokens meet 4.5:1: danger-300 `#E58A84`,
danger-600 `#962E2E`; warning-300 `#E2B45C`, warning-700 `#8A5E14`; success-300 `#5DBE88`,
success-700 `#226B43`; info-300 `#86AEDA`, info-600 `#3B6690`. The rest of each ramp is drawn
when a control first needs it.

### Semantic tokens (what widgets reference)

Widgets do **not** reference raw color. They reference tokens, which
flip cleanly between light and dark mode:

```
bg-canvas        neutral-50 / neutral-925    (content areas — rev. 1 dark value)
bg-surface       neutral-100 / neutral-900   (chrome strips — bar, dock, seams)
bg-elevated      neutral-0  / neutral-850    (card, popover, dialog — rev. 1)
border-default   neutral-200 / neutral-800
border-strong    neutral-300 / neutral-700
text-primary     neutral-900 / neutral-50
text-secondary   neutral-600 / neutral-400
text-tertiary    neutral-500 / neutral-500
text-disabled    neutral-400 / neutral-600
accent-fg        accent-600  / accent-300    (text and glyphs: links, accent labels — rev. 2, was 400 on light)
accent-mark      accent-400  / accent-300    (non-text marks: dots, bars, indicators, selected edges — rev. 2)
accent-fill      accent-500  / accent-500    (primary and filled controls at rest — rev. 2)
accent-fill-hover   accent-600 / accent-600  (rev. 2)
accent-fill-pressed accent-700 / accent-700  (rev. 2; replaces accent-pressed)
accent-bg        accent-100  / accent-700
accent-strong    accent-600  / accent-200    (hover/pressed text on accent-bg — rev. 1)
accent-pressed   accent-500  / accent-500    (rev. 1)
text-on-accent   neutral-50  / neutral-50    (labels on accent fills — rev. 1)
focus-ring       accent-400  / accent-300
terminal-bg      neutral-900 / neutral-950   (rev. 1 — terminal surfaces, both themes dark)
terminal-text    neutral-200 / neutral-300   (rev. 1)
scrim            neutral-950 @ 45% / #040608 @ 62%   (rev. 1 — overlay backdrop)
state-hover      neutral-900 @ 6%  / neutral-50 @ 8%    (rev. 2 — overlay on any surface)
state-pressed    neutral-900 @ 10% / neutral-50 @ 12%   (rev. 2)
selection-bg     accent-100  / accent-700               (rev. 2 — text selection, selected rows, focused list)
selection-idle   neutral-200 / neutral-800              (rev. 2 — the same when the window or list isn't focused)
border-control   neutral-500 / neutral-500              (rev. 2 — edges that are a control's only affordance)
bg-inverse       neutral-900 / neutral-100              (rev. 2 — tooltips, toasts)
text-inverse     neutral-50  / neutral-900              (rev. 2)
danger-fg        danger-600  / danger-300               (rev. 2 — and warning-, success-, info-fg: 700/300, 700/300, 600/300)
```

**Contrast floor** *(rev. 2)*. Text that a user reads to act is at least 4.5:1 against what it
sits on; a control's boundary, a focus ring or a mark that carries state is at least 3:1
(WCAG 2.2, 1.4.3 and 1.4.11). Rev. 2 changed the tokens that missed it, measured:

| pair | before | after |
|---|---|---|
| primary button label, light (on accent-400) | 3.24 | 4.62 (on accent-500) |
| primary button label, dark (on accent-300) | 2.28 | 4.62 (on accent-500) |
| accent text (links) on light canvas | 3.24 (accent-400) | 6.80 (accent-600) |
| danger text on dark canvas | 3.08 (danger-500) | 7.18 (danger-300) |
| warning / success text on light canvas | 2.70 / 4.10 | 5.49 / 6.23 (700 steps) |
| checkbox, radio, toggle edge on light | 1.43 (border-strong) | 3.62 (border-control) |

`text-tertiary` on light is 3.62:1, so it is for placeholders and hints only, never for
information (timestamps and metadata use `text-secondary`). `border-default` and
`border-strong` stay as they are: they separate regions, they don't identify controls.


Adding a new token requires a meaningful gap; resist the urge.
*(rev. 1 added accent-strong/-pressed, terminal-*, and scrim — all in
daily use by the shell design; each earned its slot by appearing on
three or more distinct shell surfaces.)*

**Shell-specific values** *(rev. 1)* — the desktop wallpaper is a
theme-flipped vertical gradient (light `#F4F5F7 → #E9EDF0 62% → #DFE4E9`;
dark `#14181D → #0E1114`) with a 64px grid of hairlines
(`rgba(90,99,108,.055)` light / `rgba(168,176,184,.045)` dark). These
live in the token table as `shell.wallpaper-*` / `shell.grid-line`, not
as general-purpose tokens: only the wallpaper may use them. The
signature deep teal `#0A808C` remains the first-paint clear color
(`bg-window`) — the tone shown before any surface paints; the shell
wallpaper covers it in normal operation. App identity tints (the muted
per-app glyph colors and pastel tile gradients) are **app metadata**,
not theme tokens — they ship in the app's manifest/catalog entry.

---

## §5. Shape language

Corner radius is part of the brand. Atrium's radii are **deliberately
moderate** — not Material-square, not Apple-pill.

```
radius-xs     4px    inputs, small chips, dense lists
radius-sm     6px    buttons, tags, secondary controls
radius-md     8px    cards, panels, popovers, surface cards
radius-tile  10px    dock/launcher app tiles only (rev. 1)
radius-lg    12px    dialogs, large modals
radius-xl    16px    hero surfaces (login screen panel)
radius-pill 9999px   pills, avatars, segmented control thumb
```

*(rev. 1: `radius-tile` exists because a 40px app tile at radius-md
reads square and at radius-lg reads like a pill; 10 is the optical
midpoint. It is legal on 40px app tiles and nothing else.)*

Strokes are **always 1px logical**, scaled by DPI by the GPU layer.
Borders that look heavier are layered (1px stroke + 1px inset glow),
never 2px+.

Drop-shadow policy:

- **No drop shadows by default.** Surfaces sit on the canvas; elevation
  is communicated by background tone (`bg-elevated` is *lighter* on
  light theme, *darker* on dark theme).
- One exception: floating popovers + dialogs get a single subtle
  *(rev. 2: Fresco has no shadow op yet; until it has one, floating surfaces are drawn with the
  1px `border-default` edge alone. The shadow is a rounded-rect blur op asked of the Fresco
  owners, not a texture.)*
  shadow (`0 8px 24px rgba(0,0,0,0.12)`) for clarity-of-detachment.
- Two shadows stacked is always wrong. One or zero.

---

## §6. Motion

**Spring physics by default** (locked design decision 2026-05-04).

Two spring presets cover ~90% of the toolkit:

```
spring-snappy    stiffness=400  damping=30  mass=1   feedback, quick state changes
spring-gentle    stiffness=200  damping=22  mass=1   layout transitions, content moves
```

Easing curves are reserved for cases where springs don't fit
(deterministic-duration animations like loading bars):

```
ease-standard   cubic-bezier(0.2, 0, 0.2, 1)   default ease for tweens
ease-emphasized cubic-bezier(0.05, 0.7, 0.1, 1) for entrance emphasis
```

Duration scale (only used for non-spring animations):

```
fast      120ms   tap feedback, hover state
medium    200ms   element transitions
slow      350ms   page-level transitions
xslow     600ms   intro animations (login, app launch)
```

**Motion principles:**

- **Direction conveys causality.** Things slide in from the direction
  they came from; out toward where they go. No random fade-only
  transitions.
- **Vector-native motion.** Animate *shape parameters* — corner radius,
  stroke width, path control points — not just position/opacity. We
  can do this; bitmap toolkits can't. Showing it off (subtly)
  signals "this is a different kind of toolkit."
- **Respect reduced-motion.** A user setting collapses springs to
  120ms ease + 50% travel distance. Toolkit-level, never per-app.

---

## §7. Vector-native principles

We render every widget through Fresco's GPU pipeline. This buys us
things bitmap toolkits don't have. Use them, don't hide them.

1. **Crisp at any DPI.** No 1× / 2× / 3× asset duplication. No "looks
   blurry on this monitor." Lines stay 1px logical at every scale.
2. **Shape morphing.** A button can transition into a loading spinner
   by morphing path control points, not crossfading two bitmaps.
   Subtle uses everywhere; not gimmicky.
3. **Real curves.** Where a curve is called for (chart line, slider
   track end, icon stroke), it's a Bézier, not a stair-stepped
   approximation. This is invisible to a casual user but the eye
   registers "this feels right."
4. **Sub-pixel positioning.** Layout can use fractional positions when
   it produces better optical alignment. The GPU handles AA.
5. **Stroke-as-data.** Width, dash pattern, line cap, miter limit are
   all live properties. Animate them. A focus ring that "draws on" by
   stroke-dashoffset feels native to vector and impossible in bitmap.

What we *don't* do, even though we could:

- ❌ Heavy gradients that exist for ornament. One subtle gradient per
  surface, max — and only when it serves a function (e.g. distinguishing
  a press state).
- ❌ Glassmorphism / acrylic blur. Tech-trendy and doesn't survive a
  redesign cycle.
- ❌ Showing off vector capabilities for their own sake. Restraint
  reads as confidence.

---

## §8. Density

**Comfortable, not roomy.** Closer to Linear than to macOS Sequoia.

Reference dimensions:

```
button height (default)       32px
button height (compact)       24px
button height (large)         40px
input height (default)        32px
list row height (default)     32px
list row height (dense)       24px
menu item height              28px
toolbar height                40px
title bar height              32px
```

**Shell chrome dimensions** *(rev. 1 — fixed by the Forum shell design;
these are shell landmarks, not general control sizes)*:

```
forum-bar height              38px
seam height                   28px   (the Forum-owned strip on every surface)
dock rail width               56px
dock tile                     40px   (radius-tile)
surface chip height           24px
workspace chip height         26px
shell button height           28px   (dense-tier button inside shell popovers)
```

Note the shell deliberately runs 2–4px tighter than the app-side
reference dimensions (38 vs 40 toolbar, 28 vs 32 title bar) — same
argument as the dense type tier. Apps use the reference table; the
shell uses these. The dock-tile hover treatment is the 2px translate
only — **no drop shadow** (the shadow in early shell mockups is
rescinded; the surface-shadow policy in §5 stands unamended).

Touch target accommodation: when input is touch-detected, all
interactive elements scale by 1.25× automatically. Toolkit-level,
not per-widget.

---

## §9. Iconography

**Phosphor Icons** (MIT licensed) as the system icon set.
Consistent stroke weight (1.5px at 16px target size), geometric,
slightly humanist. Already used by major modern systems.

Custom icons we ship (Atrium-specific glyphs — login, lock, crown
for system, etc.) match Phosphor's stroke weight and geometric
language.

We do **not** mix icon sets. One family, system-wide.

Icon sizes (matched to type scale):

```
icon-xs    12px   inline with caption text
icon-sm    16px   default — UI text, button glyphs
icon-md    20px   list rows, menu items
icon-lg    24px   toolbar, primary nav
icon-xl    32px   app icons in launcher
icon-2xl   48px   feature icons, hero illustrations
```

---

## §10. How this shows up in the toolkit

Every Pergola widget references **only**:

- Spacing tokens (`spacing.md`)
- Type tokens (`type.md`, `type.weight.medium`)
- Color tokens (`color.text-primary`)
- Radius tokens (`radius.sm`)
- Motion tokens (`motion.spring-snappy`)

A widget that references a raw value (`16px`, `#F2F4F6`, `200ms`)
fails review. The whole point of the design language is that a future
theme refresh changes one file (the token table), not 200 widgets.

The token table lives at `pergola/src/theme/tokens.rs` *(rev. 2: there is no separate
`pergola-theme` crate)*. Light theme is the default; dark theme is an alternative
loaded at the same surface.

---

## §11. What's deferred

- **Application-specific theming** (custom accent colors, alternate
  type families) — possible via theme extension, but the system shell
  always uses Atrium's defaults. Apps bringing their own theme is a
  D5+ feature.
- **High-contrast + accessibility-targeted themes** — beyond the
  default light/dark pair. Locked-in tokens make this a "one new
  table" addition, not a retrofit.
- **Localization-driven type scale shifts** (CJK, Arabic) — the scale
  values may shift slightly when the type family changes for those
  scripts. Plex covers Latin/Cyrillic/Greek and has CJK companions
  (Plex Sans JP, KR, TC, SC) that we'll bring in when localization
  starts.
- **Animation choreography for full screens** (login enter, app
  launch, workspace switch) — these compose primitive motions and
  belong in the apps themselves, not the language doc.

---

## §12. Reference render plan

Before any widget code, Pergola produces one screen via
`frescod-vulkan-smoke` showing the language standing alone:

1. A panel with `bg-elevated` background, `radius-md` corners
2. Heading text (2xl, semibold, `text-primary`)
3. Body paragraph (sm, regular, `text-primary` and `text-secondary`)
4. A row of three buttons: primary (`accent-bg` fill), secondary
   (border + `text-primary`), ghost (text only)
5. A text input with placeholder and focus state
6. A focus ring transitioning between elements via `spring-snappy`

If that one screen reads correctly to the eye standing alone,
the language is right and we proceed to widget construction. If it
reads wrong, we iterate the tokens until it doesn't — *before*
shipping a hundred widgets that bake in a not-quite-right palette.

This render is a one-day effort and saves a month of toolkit churn.

---

## §13. Living document

This doc is normative for the system shell. Apps may diverge with
deliberate intent (e.g., a creative app with its own brand). Updates
to this doc require:

- A justification (what the current choice fails at)
- A render comparing before/after on the §12 reference screen
- Approval from the Pergola track owner

We expect ~3 revisions in the first year as real apps stress-test the
tokens, then long stability.

---

## §14. Controls *(rev. 2)*

Every control is built from the same parts, so the desktop reads as one system: a **body**
(fill and edge), a **label** (Sans, medium weight, `md` in apps and `sm` in the shell), an
optional **glyph** (`icon-sm`, 4px from the label), and the **focus ring**. Sizes come from §8;
no control invents its own height.

### §14.1 States, one rule for all

| state | how it shows |
|---|---|
| rest | the control's own fill and edge |
| hover | `state-hover` over the body (filled controls: `accent-fill-hover`); pointer only, never on touch |
| pressed | `state-pressed` over the body (filled: `accent-fill-pressed`); the label does not move |
| focused (keyboard) | the focus ring; pointer focus shows no ring |
| selected / on | `selection-bg` body, or the control's own on-look (checkbox, toggle, radio) |
| disabled | label `text-disabled`, edge `border-default`, no hover or press; never faded with alpha |
| invalid | edge and message in `danger-fg`, message below the control in `xs` |

State changes colours, never size or position: a control's node count and box are the same in
every state. Hover and press take `duration.fast`; the focus ring draws on with `spring-snappy`.

**The focus ring** is the one Pergola already draws on Button: a 1px `focus-ring` stroke 1px
outside the body, and a 2px `accent-bg` halo outside that (3px from the body in all), with the
body's radius plus the offset. It shows on whatever has keyboard focus, and only one thing on
screen has it.

### §14.2 The controls

| control | anatomy | on / selected |
|---|---|---|
| **Button** primary | `accent-fill`, label `text-on-accent`, radius-sm, padding 12 | — |
| Button secondary | 1px `border-strong`, fill `bg-elevated`, label `text-primary` | — |
| Button ghost | no edge or fill, label `text-secondary` | — |
| Button destructive | `danger-500` fill, label `text-on-accent` (5.69:1) | — |
| **Text field** | 32 high, fill `bg-canvas`, 1px `border-strong` (hover `border-control`), radius-xs, text 12 from the edge; caret 1.5px `accent-fg`, blinks 530 ms, still while typing; selection `selection-bg` | — |
| Text area | the text field, many lines, `line-height.body`, its own ScrollView | — |
| **Checkbox** | 16 box, 1px `border-control`, radius-xs, label 8 to its right; the hit area is box plus label | `accent-fill` box, check glyph `text-on-accent`; mixed: a 8×2 bar |
| **Radio** | 16 circle, 1px `border-control` | `accent-fill` circle, 6 `text-on-accent` dot |
| **Toggle** | 32×18 pill track, 1px `border-control`, 14 thumb `border-control` | track `accent-fill`, thumb `text-on-accent`, slides with `spring-snappy` |
| **Slider** | 4 high pill track `border-strong`, filled part `accent-mark`, 16 thumb `bg-elevated` with 1px `border-control` | — |
| **Dropdown** (select) | the secondary button's body, label left, `caret-down` glyph right; opens a menu aligned to its left edge, the current item under the pointer | — |
| **Menu button** | the secondary button's body, label left, `caret-down` right; opens a menu of actions 4 below its left edge (a pop-up window) | held in the pressed state while its menu is open |
| Menu button, **attached** (trusted surfaces: the Forum bar) | as the menu button | while open the whole button is `accent-fill` (hover `accent-fill-hover`), label and caret `text-on-accent`, the caret turned up; the menu hangs from it with no gap, inside a 2px `accent-fill` edge instead of `border-default`, at least as wide as the button. High contrast: the same, with `accent-fill` replaced by the high-contrast accent and a 2px `text-primary` outline around the button |
| **Segmented control** | `bg-surface` track with a 1px `border-default` edge (so it shows on `bg-surface` too), radius-sm, 2px inset; segments equal width | the selected segment is a `bg-elevated` pill with `border-default` |
| **Tabs** (in a panel) | labels `text-secondary` 16 apart, a 2px `accent-mark` bar under the selected one, a 1px `border-default` line under the row | label `text-primary` |
| **List row** | 32 (dense 24), padding 12, glyph `icon-md` | `selection-bg` when the list has focus, `selection-idle` when not |
| **Menu** | floating surface, 4px inset; items 28 high, padding 12, shortcut right in `text-tertiary`; separator a `Divider` with 4px above and below; submenu `caret-right` | the hovered or arrowed item takes `selection-bg` |
| **Tooltip** | `bg-inverse`, text `text-inverse` `sm`, radius-xs, padding 4×8, 8 from the target; shows after 500 ms of hover, at once while another is showing, never on touch | — |
| **Popover** | floating surface, padding 16, radius-md | — |
| **Group box** | `bg-elevated`, 1px `border-default`, radius-md, padding 16; title `sm` semibold `text-secondary`; contents 12 apart | — |
| **Section** | no frame; title `md` semibold `text-primary`, optional `sm` `text-secondary` line 4 below; contents 12 below; sections 24 apart | — |
| **Form** | two columns: labels `md` `text-primary` at their natural width, fields fill the rest, 12 between, rows 8 apart, each row on one baseline | — |
| **Dialog / sheet** | floating surface, radius-lg, padding 24, `scrim` behind; buttons bottom right, primary last; Esc and the scrim cancel | — |
| **Scrollbar** | overlay, no track: a 6 wide pill, `neutral-500` at 50%, 2 from the edge; shows while scrolling and 800 ms after, widens to 10 on hover | — |
| **Progress** | the existing `ProgressBar`: 4 high pill, `accent-mark` on `bg-surface`; indeterminate is a moving third | — |

**Why the attached look** (fresco-popups.md): a menu is a pop-up window frescod places, and an
app can draw a fake menu in its own window just under the bar. Apps cannot draw in the bar, so the
tell is the bar: a real bar menu always hangs from a lit bar button, joined to it in its colour,
and only the bar's own menus are drawn that way. frescod also stacks app pop-ups below the bar
and its pop-ups, and while a menu is open only its owner can open more. Apps' menu buttons and
drop-downs never use the attached look (Pergola offers it only to trusted surfaces by
convention; a fake one in an app's window still sits under an unlit bar).

A **floating surface** (menu, popover, dialog, tooltip excepted) is `bg-elevated`, 1px
`border-default`, and the one §5 shadow once Fresco draws it. Floating surfaces open with
`spring-snappy`, 4px of travel from the direction of what opened them, and close by fading in
`duration.fast`.

### §14.3 Keyboard, the same everywhere

Tab and Shift-Tab move focus in reading order; arrows move within a group (radios, segments,
tabs, menus, lists). Space toggles or presses; Enter presses the focused button, or the
dialog's primary one. Esc closes the topmost floating surface, and only that. A menu opened by
keyboard has its first item selected; one opened by pointer has none.

### §14.4 Trusted surfaces

Surfaces that another app must not be able to imitate are drawn by their own service and marked
by frescod, not by Pergola: the credential slot's gold ring and key tab (arca-autofill §4.1), the
origin strip, and the Vestibulum lock screen. Inside them the controls follow this section, so
Arca's field is a text field with Atrium's caret, selection and focus ring.

## §15. Appearance, set once for the desktop *(rev. 2)*

Light, dark, or follow the time of day is **one system setting**, not a per-app choice. frescod
tells every client when it changes (a display event, the same path as a scale change), Pergola
swaps `Semantic::LIGHT` and `DARK` and redraws, and every app follows in the same frame. Apps do
not read an environment variable and do not offer their own switch; the Forum bar's toggle sets
the system setting. **Themes** (decided by Shankar, 2026-11-06): Atrium ships light and dark, a high-contrast
table for each (§11), and a short curated list of accent colours the user picks from, each
held to the §4 contrast floor in every table (fill, text-on-fill, accent text, marks and the
focus ring). Amber-bronze stays the default. There are no installable themes, and nothing
themes the trusted surfaces of §14.4. High contrast and the accent list follow the controls.

## §16. Where the code drifts today *(rev. 2, 2026-11-06)*

What a review of Pergola, the Forum shell and arca-ui found against this document (struck through as fixed):

| where | drift | fix |
|---|---|---|
| Pergola `Button` primary | rests on accent-400 (label 3.24:1); hover goes to accent-600, darker than pressed (500) | ~~`accent-fill`, `-hover`, `-pressed`~~ fixed |
| Pergola `TextField` | no edge at all; no caret or selection | ~~§14.2 text field~~ fixed |
| Pergola hover and press | each widget picks a neighbouring tone (`bg-surface`) | ~~`state-hover` and `state-pressed` overlays~~ fixed |
| forum-bar, forum-dock | theme from an environment variable; the bar's toggle changes only the bar | ~~§15~~ fixed: every Pergola window, the dock and the overview take frescod's appearance; the bar's switch sets it through forum-wm |
| forum-shelf | always light | ~~§15~~ fixed (follows through `Window`) |
| arca-ui | draws raw RGB, with a blue focus edge and caret (the "tech blue" §1 rejects) | ~~Pergola's token table (arca-ui can depend on the theme module alone)~~ fixed by the Arca thread (c7638ea) |
| floating surfaces | no shadow op in Fresco | edge only until the op exists (§5) |
| motion | springs are specified but Pergola has no frame clock, so nothing animates | the frame clock in Pergola's order of work |
| §12 reference render | rev. 1's before/after render was never captured; rev. 1 approval pending | the control sheet below, then a Pergola render of it |

**The control sheet.** Rev. 2 is reviewed on a sheet that shows every control in §14.2 in every
state of §14.1, light and dark, drawn from this table's values. Once the controls are built,
the same sheet is rendered by Pergola through frescod and compared with it.

---

## Revision log

**Rev. 1 — 2026-08-07 — dense-shell reconciliation.** Trigger: the
high-fidelity Forum shell design (the `Atrium Shell.dc.html` handoff)
stress-tested the tokens exactly as §13 anticipated, and drifted from
this doc in ways that were partly deliberate density decisions and
partly gaps in the original tables. Changes: neutral ramp gains 0/850/925
(elevation was invisible on light — 50-on-50, a live bug found in
Pergola first-light — and too coarse on dark); accent ramp gains
700/800; `bg-elevated`→0/850, `bg-canvas` dark→925; new semantic tokens
accent-strong, accent-pressed, terminal-bg/-text, scrim; shell wallpaper
gradient + grid-line recorded as shell-scoped values; deep teal
reclassified as first-paint `bg-window`; dense-shell type tier (13px
UI, Mono half-steps) and shell chrome dimensions (bar 38, seam 28,
dock 56/40, chips 24/26) recorded; `radius-tile 10` added; type-scale
2xs 10px added; grid rule scoped to between-element spacing
(control-internal metrics are the control designer's); the mockups'
dock-tile shadow **rejected** (shadow policy unamended). Render:
`forum-demo` §12 reference screen, light + dark, before/after —
see `scratch/m1-reference-render/` when captured. Approval: given with rev. 2
(2026-11-06).

**Rev. 2 — 2026-11-06 — controls, contrast, appearance.** Trigger: the toolkit needs its
remaining controls (menus, choice controls, tabs, text editing), and the document said how
tokens look but not how a control does. Changes: §14 (one state model, the focus ring as built,
anatomy of every control, keyboard rules, trusted surfaces); §15 (appearance is one system
setting); §16 (drift found in the code); a contrast floor, with five token changes that missed it
(primary fill accent-400 → accent-500, accent text 400 → 600 on light, `-fg` steps for status
text, `border-control` for control edges); new tokens `accent-mark`, `accent-fill`
(`-hover`, `-pressed`, replacing `accent-pressed`), `state-hover`, `state-pressed`,
`selection-bg`, `selection-idle`, `border-control`, `bg-inverse`, `text-inverse`, `danger-fg`
and its siblings; the floating shadow waits for a Fresco op; the token table's real path. Render: the rev. 2 control sheet, light
and dark. Approval: Shankar, 2026-11-06, on the control sheet (which also stands as rev. 1's
render), with the darker `border-control` edges.

