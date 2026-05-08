# Atrium visual language

> *The aesthetic foundation that every Pergola widget, every Atrium app,
> and every system surface inherits.*
> Locked: 2026-05-09 · Owner: Pergola track · Reviewed before phase 0 of
> toolkit implementation.

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
xs    11px   captions, secondary metadata
sm    13px   body text, dense data
md    15px   UI default — buttons, fields, menu items
lg    18px   section leads, dialog body
xl    22px   subsection headings (h4)
2xl   28px   panel headings (h3)
3xl   36px   screen headings (h2)
4xl   48px   hero / login heading (h1)
```

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

---

## §4. Color

A grayscale-dominant palette. Color carries meaning, not decoration.

### Neutrals (12-step ramp, slightly cool)

A cool slate ramp — perceptually linear in lightness. Slight blue
undertone (~210° hue) so neutrals feel like stone, not warm beige.

```
neutral-50    #FAFBFC    page background, light
neutral-100   #F2F4F6    surface raised by 1
neutral-200   #E4E8EC    subtle dividers
neutral-300   #CFD5DA    enabled-state borders
neutral-400   #A8B0B8    placeholder text, disabled controls
neutral-500   #7C858E    secondary text
neutral-600   #5A636C    tertiary text
neutral-700   #3F484F    body text on light surfaces
neutral-800   #2A3137    headings on light surfaces
neutral-900   #181C20    primary text on light surfaces (rarely full black)
neutral-950   #0E1114    extreme contrast — rarely used
```

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

### Semantic tokens (what widgets reference)

Widgets do **not** reference raw color. They reference tokens, which
flip cleanly between light and dark mode:

```
bg-canvas        neutral-50 / neutral-950
bg-surface       neutral-100 / neutral-900
bg-elevated      neutral-50 / neutral-800       (surface raised — card, popover)
border-default   neutral-200 / neutral-800
border-strong    neutral-300 / neutral-700
text-primary     neutral-900 / neutral-50
text-secondary   neutral-600 / neutral-400
text-tertiary    neutral-500 / neutral-500
text-disabled    neutral-400 / neutral-600
accent-fg        accent-400  / accent-300
accent-bg        accent-100  / accent-700
focus-ring       accent-400  / accent-300
```

Adding a new token requires a meaningful gap; resist the urge.

---

## §5. Shape language

Corner radius is part of the brand. Atrium's radii are **deliberately
moderate** — not Material-square, not Apple-pill.

```
radius-xs     4px    inputs, small chips, dense lists
radius-sm     6px    buttons, tags, secondary controls
radius-md     8px    cards, panels, popovers
radius-lg    12px    dialogs, large modals
radius-xl    16px    hero surfaces (login screen panel)
radius-pill 9999px   pills, avatars, segmented control thumb
```

Strokes are **always 1px logical**, scaled by DPI by the GPU layer.
Borders that look heavier are layered (1px stroke + 1px inset glow),
never 2px+.

Drop-shadow policy:

- **No drop shadows by default.** Surfaces sit on the canvas; elevation
  is communicated by background tone (`bg-elevated` is *lighter* on
  light theme, *darker* on dark theme).
- One exception: floating popovers + dialogs get a single subtle
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

The token table lives at `pergola-theme/src/tokens.rs` once the crate
exists. Light theme is the default; dark theme is an alternative
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
