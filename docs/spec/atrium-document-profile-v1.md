# Atrium Document Profile v1 — a layout language with guarantees

**Status:** draft, not built. Refines [atrium-navigator-backend.md](atrium-navigator-backend.md) §6,
which required this document to exist before M2 (its gate — a conformance *number* — is
meaningless without an enumerated target). Parent thesis:
[atrium-navigator.md](atrium-navigator.md).

---

## 0. The framing: not a CSS subset

The obvious way to write this document is "which CSS properties do we support". That
produces a smaller browser, which is not interesting — a smaller browser is a worse
browser with the same pathologies.

The useful framing is the inverse: **the profile is a layout language defined by the
properties it guarantees**, and the feature list is whatever survives those guarantees.
Where a CSS feature cannot be admitted without breaking a guarantee, it is excluded and
the exclusion is the point, not a gap to be filled in v2.

| | guarantee | what it kills |
|---|---|---|
| **G1** | **Determinism** — same bytes → bit-identical scene graph | flaky rendering, untestable layout |
| **G2** | **Arrival-order independence** — layout does not depend on when resources arrive | layout shift (CLS), FOUC, font swap |
| **G3** | **Boundedness** — a conforming document has a static resource ceiling | decompression bombs, pathological nesting, OOM |
| **G4** | **Local reasoning** — a rule's effect is determinable from a bounded context | specificity wars, action at a distance |
| **G5** | **Intrinsic semantics** — the semantic tree is produced, not derived | accessibility as a bolt-on that drifts |
| **G6** | **Zero ambient authority** — the document observes nothing about its host | fingerprinting, tracking, cross-site state |

Each guarantee is a test in the headless harness (§7), not an aspiration. Several are
properties no shipping browser can claim, and two of them (G2, G6) are testable in a way
that produces a hard pass/fail rather than a judgement call.

---

## 1. Which browser pathologies this actually fixes

Worth being precise about what is structural here versus what is merely tidier, because
the second kind does not justify a new engine.

### 1.1 Fixed by the architecture (JS-free + jailed + fixed environment)

**Fingerprinting.** The browser fingerprinting surface exists because the page runs code
that can measure its host: fonts installed, canvas rasterization quirks, WebGL strings,
timers, screen metrics, plugin lists. Remove the script and the *measurement vehicle* is
gone, not merely restricted. Combined with a shipped, versioned font set (no system font
enumeration, because system fonts are never consulted), the render path has no channel to
observe the machine. This is testable as an equality: §7's G6 test renders the same
document on two hosts differing in installed fonts, time zone, hostname and locale, and
requires **bit-identical scene graphs**.

**Cross-site state and cache probing.** Documents receive zero capabilities, so there is
no cookie jar, no local storage, no service worker, no cross-site state to partition —
it cannot exist rather than being disabled. Cache-probing likewise loses its vehicle;
the residual server-observable possession oracle is handled in the backend spec §5.1 and
is not a document-lane concern.

**Layout thrashing and forced synchronous reflow.** A JS problem by construction. Absent.

**Same-process cross-document leakage.** One jail per document (backend spec §2).

### 1.2 Fixed by the profile (choices available to any engine, but not to a compatible one)

**Layout shift.** CLS is a genuine, measured plague, and its cause is structural: layout
begins before the inputs to layout have arrived, so boxes move when images and fonts land.
The profile makes this impossible rather than discouraged — replaced content **must**
carry intrinsic dimensions or an `aspect-ratio`, and fonts are local, so there is no
metric change to reflow around. G2 is then a testable invariant: permute resource arrival
order, assert not just the same final layout but that **no box ever occupies two
positions**. A browser cannot adopt this rule without breaking the existing web; we have
no existing web to break.

**Margin collapsing.** Universally regretted, a perennial source of "why is there a gap".
Margins in this profile are literal and never collapse.

**Floats and the containing-block maze.** Excluded. Flex and grid cover the layouts floats
were pressed into, without the block-formatting-context rules that make float behaviour
unpredictable.

**The cascade.** CSS's own designers shipped `@layer` to patch cascade unpredictability,
which is an admission. The profile drops `!important` entirely, drops the general
descendant combinator (the main engine of action-at-a-distance), and orders declarations
by explicit layer then source order. The resolution remains deterministic — CSS's already
is — but becomes *locally reasonable*, which is the actual complaint.

**Error recovery.** HTML5 parsing is specified as twenty years of bug-compatible error
recovery, and it is one of the largest and most security-sensitive parts of a browser.
The profile parses **strictly and fails loudly**. Tolerance is not eliminated but
*quarantined*: see §6.

**Animation and transitions.** Excluded from v1. They are a determinism hazard, an energy
cost, and a motion-sensitivity concern, and they are not required to read a document.

### 1.3 Not fixed — honest limits

These are hard for browsers because they are hard, and nothing here makes them easier:

- **International text.** Bidi (UAX #9), line breaking (UAX #14), and complex-script
  shaping are required, not optional, and must be done properly. Candidate dependencies
  (HarfBuzz-class shaping, ICU-class segmentation) are permissively licensed, but each
  needs the usual licence check against LANGUAGE-POLICY before adoption. There is no
  shortcut and no subset that serves non-Latin readers honestly.
- **Legacy content.** Strict parsing plus no floats plus no margin collapsing means real
  pages off today's web do not render as authored. §6 is the answer, and it is a
  conversion story, not a compatibility one.
- **Accessibility beyond the tree.** G5 makes the semantic tree intrinsic and testable,
  which removes a whole class of drift. It does not deliver a screen-reader experience;
  that is a separate body of work.
- **Media.** Video and audio are a large independent surface (containers, codecs, timing,
  DRM pressure). Out of scope for v1.
- **Print and pagination.** Deliberately deferred, but noted as a place where a
  from-scratch engine could be *better* than browsers rather than merely different, since
  paged layout would not be an afterthought.

---

## 2. Document model

**Input grammars.** Markdown (CommonMark subset, with a defined table extension) and a
strict, well-formed HTML subset. A document that does not parse is an error with a
position, never a silent recovery.

**Excluded elements, with reasons:** `<script>` (no scripting), `<iframe>`/`<object>`/
`<embed>` (composition is Limen's job, at the jail boundary where it belongs), `<canvas>`
(a scripting surface), event-handler attributes, `<style>` `@import` chains beyond a
bounded depth, `<base>` (ambient rewriting of every link).

**Structural and semantic elements** are the core of the subset, and they are what feeds
G5: sectioning (`article`, `section`, `nav`, `aside`, `header`, `footer`, `main`),
headings `h1`–`h6`, `p`, lists, `figure`/`figcaption`, `blockquote`, `table` and its
parts, `a`, `img`, inline emphasis and code, `details`/`summary`, and definition lists.

**Forms** exist but hold no authority. A document may *declare* a form; submission is a
user-initiated action performed by `navigatord`, not by the document — the document has
no network capability with which to submit anything. This keeps search boxes and the
like working without granting the document a channel.

---

## 3. The property set

### 3.1 Fixed rules (not properties — no opt-out)

| rule | value |
|---|---|
| box model | `border-box`, always |
| margin collapsing | never |
| floats | none |
| colour space | sRGB; compositing defined in premultiplied linear, stated normatively so G1 is reproducible across implementations |
| text direction | per-element, UAX #9 bidi |

### 3.2 How to read this section

**Every property is listed with its admitted values, initial value, and whether it
inherits.** A conformance run scores against this list, so anything absent is absent by
decision, not oversight, and a producer that emits it gets a diagnostic (§5.1).

Three conventions, each load-bearing:

1. **Longhands only — no shorthands in v1.** `margin`, `border`, `background`, `flex`,
   `grid-area`, `font` and friends are *excluded*. A shorthand gives one value several
   spellings, and the hardening spec requires exactly one encoding per value for content
   addressing to be sound; shorthand expansion is also a classic parser-differential
   surface. Authoring tools can expand them; the profile does not parse them.
2. **CSS-wide keywords: `inherit` and `initial` only.** `unset`, `revert` and
   `revert-layer` are excluded — they are defined in terms of cascade origins, which is
   the machinery §3.7 deliberately flattens.
3. **`auto` is a distinct value, never a synonym.** Where it appears its meaning is given.

`<length>` = a number with unit `px`, `em`, `rem`, `ch`, `vw`, `vh` (§3.6). `<percentage>`
resolution basis is stated per property. `<color>` per §3.6.

### 3.3 Box and layout

| property | admitted values | initial | inh. |
|---|---|---|---|
| `display` | `block` \| `inline` \| `inline-block` \| `flex` \| `grid` \| `table` \| `table-row` \| `table-cell` \| `none` | `inline` | no |
| `position` | `static` \| `relative` \| `absolute` | `static` | no |
| `top` `right` `bottom` `left` | `<length>` \| `<percentage>` \| `auto` | `auto` | no |
| `width` `height` | `<length>` \| `<percentage>` \| `auto` \| `min-content` \| `max-content` | `auto` | no |
| `min-width` `min-height` | `<length>` \| `<percentage>` \| `auto` | `auto` | no |
| `max-width` `max-height` | `<length>` \| `<percentage>` \| `none` | `none` | no |
| `aspect-ratio` | `<number>` \| `auto` | `auto` | no |
| `margin-top` `-right` `-bottom` `-left` | `<length>` \| `<percentage>` \| `auto` | `0` | no |
| `padding-top` `-right` `-bottom` `-left` | `<length>` \| `<percentage>` (non-negative) | `0` | no |
| `border-*-width` (4) | `<length>` (non-negative) | `0` | no |
| `border-*-style` (4) | `none` \| `solid` \| `dashed` \| `dotted` | `none` | no |
| `border-*-color` (4) | `<color>` | `currentColor` | no |
| `border-*-radius` (4 corners) | `<length>` \| `<percentage>` | `0` | no |
| `overflow-x` `overflow-y` | `visible` \| `hidden` \| `auto` \| `scroll` | `visible` | no |
| `isolation` | `isolate` \| `auto` | `auto` | no |
| `z-index` | `<integer>` \| `auto` | `auto` | no |

Percentages resolve against the containing block's inline size for `width`,
`margin-*`, `padding-*` and `left`/`right`; against its block size for `height` and
`top`/`bottom`.

**`box-sizing` is not a property** — `border-box` is a fixed rule (§3.1). **`float` and
`clear` are absent.** **`position: fixed`/`sticky`** are deferred (both interact with
scrolling and compositing). **`z-index` creates no implicit stacking context**: a
stacking context exists only where `isolation: isolate` says so, which is why `isolation`
is admitted at all.

### 3.4 Flex

| property | admitted values | initial | inh. |
|---|---|---|---|
| `flex-direction` | `row` \| `column` | `row` | no |
| `flex-wrap` | `nowrap` \| `wrap` | `nowrap` | no |
| `justify-content` | `flex-start` \| `flex-end` \| `center` \| `space-between` \| `space-around` \| `space-evenly` | `flex-start` | no |
| `align-items` | `stretch` \| `flex-start` \| `flex-end` \| `center` \| `baseline` | `stretch` | no |
| `align-self` | `auto` \| (as `align-items`) | `auto` | no |
| `align-content` | (as `align-items`) | `stretch` | no |
| `row-gap` `column-gap` | `<length>` \| `<percentage>` | `0` | no |
| `flex-grow` `flex-shrink` | `<number>` (non-negative) | `0` / `1` | no |
| `flex-basis` | `<length>` \| `<percentage>` \| `auto` \| `content` | `auto` | no |

**Excluded: `order`, `row-reverse`, `column-reverse`, `wrap-reverse`** — all four decouple
visual order from reading order. `order` is the well-known case; the `*-reverse` values do
the same thing by another route, and admitting them while excluding `order` would be
incoherent. G5 makes the resulting defect visible rather than ignorable, so the profile
declines to create it.

### 3.5 Grid

| property | admitted values | initial | inh. |
|---|---|---|---|
| `grid-template-columns` `-rows` | `none` \| `<track-list>` | `none` | no |
| `grid-auto-columns` `-rows` | `<track-size>` | `auto` | no |
| `grid-auto-flow` | `row` \| `column` | `row` | no |
| `grid-row-start` `-end` `grid-column-start` `-end` | `auto` \| `<integer>` \| `span <integer>` | `auto` | no |
| `justify-items` `align-items` (grid) | `stretch` \| `start` \| `end` \| `center` | `stretch` | no |
| `justify-self` `align-self` (grid) | `auto` \| (as above) | `auto` | no |

`<track-size>` = `<length>` \| `<percentage>` \| `<number>fr` \| `min-content` \|
`max-content` \| `minmax(<track-size>, <track-size>)`.
`<track-list>` = one or more `<track-size>` or `repeat(<integer>, <track-list>)`, with the
repeat count and total track count bounded by §3.12.

**Excluded:** named grid lines and areas (a second naming system over the same geometry),
`subgrid`, and `grid-auto-flow: dense` — dense packing reorders items relative to document
order, the same objection as `order`.

### 3.6 Values

**Units.** `px`, `em`, `rem`, `ch`, `vw`, `vh`, `%`, and `fr` (grid tracks only).
`em`/`ch` resolve against the element's own computed font; `rem` against the root's.

**`calc()`** over `+ - * /` with bounded nesting depth (§3.12). Division by zero, and any
expression mixing incompatible units, is a diagnostic rather than a clamp.

**Custom properties** `--*`: inherited, substituted via `var(--name, <fallback>)`, with
cycle detection and bounded substitution depth (§3.12). A cycle is a diagnostic.

**`<color>`** = `#rgb` / `#rrggbb` / `#rrggbbaa`, `rgb()`, `rgba()`, `hsl()`, `hsla()`,
`currentColor`, `transparent`, or a profile-named colour. All sRGB; compositing in
premultiplied linear per §3.1. **Excluded:** system colours (they leak host configuration,
which G6 forbids), and `color()`/wide-gamut functions in v1.

**`<number>`, `<integer>`, `<percentage>`** are finite decimals; NaN and infinities have
no syntax and are rejected at parse, which is the authoring-side half of the hardening
spec's numeric-domain rule.

### 3.7 Typography

| property | admitted values | initial | inh. |
|---|---|---|---|
| `color` | `<color>` | profile default | **yes** |
| `font-family` | a profile-named stack | profile default | **yes** |
| `font-size` | `<length>` \| `<percentage>` | `16px` | **yes** |
| `font-weight` | `100`…`900` (hundreds), limited to weights the named stack ships | `400` | **yes** |
| `font-style` | `normal` \| `italic` | `normal` | **yes** |
| `line-height` | `<number>` \| `<length>` \| `<percentage>` | `1.5` | **yes** |
| `letter-spacing` `word-spacing` | `<length>` | `0` | **yes** |
| `text-align` | `start` \| `end` \| `center` \| `justify` | `start` | **yes** |
| `text-indent` | `<length>` \| `<percentage>` | `0` | **yes** |
| `text-decoration-line` | `none` \| `underline` \| `line-through` | `none` | no |
| `text-decoration-color` | `<color>` | `currentColor` | no |
| `text-transform` | `none` \| `uppercase` \| `lowercase` \| `capitalize` | `none` | **yes** |
| `white-space` | `normal` \| `pre` \| `pre-wrap` \| `nowrap` | `normal` | **yes** |
| `overflow-wrap` | `normal` \| `break-word` | `normal` | **yes** |
| `tab-size` | `<integer>` | `8` | **yes** |
| `font-variant-numeric` | `normal` \| `tabular-nums` | `normal` | **yes** |
| `direction` | `ltr` \| `rtl` | `ltr` | **yes** |

★ **`text-transform` is locale-dependent** (Turkish dotted/dotless i, Greek final sigma).
It is admitted, and the profile must name the casing locale explicitly rather than
inheriting one from the host — a host locale would be an ambient input and would break G6
and G1 together.

**Excluded:** web fonts (v1), `hyphens` (needs dictionaries), `font-size-adjust`,
`unicode-bidi` (UAX #9 plus `direction` is the whole model), and font-size *keywords*
(`medium`, `large`, …) which add a UA-defined table for no expressive gain.

### 3.8 Paint

| property | admitted values | initial | inh. |
|---|---|---|---|
| `background-color` | `<color>` | `transparent` | no |
| `background-image` | `none` \| `url()` \| `linear-gradient(…)` | `none` | no |
| `background-position-x` `-y` | `<length>` \| `<percentage>` \| `left`/`center`/`right` (resp. `top`/`center`/`bottom`) | `0%` | no |
| `background-size` | `auto` \| `cover` \| `contain` \| `<length>` \| `<percentage>` | `auto` | no |
| `background-repeat` | `repeat` \| `repeat-x` \| `repeat-y` \| `no-repeat` | `repeat` | no |
| `opacity` | `<number>` clamped to 0–1 | `1` | no |
| `visibility` | `visible` \| `hidden` | `visible` | **yes** |
| `box-shadow` | `none` \| offset-x offset-y blur spread `<color>`, blur and spread bounded by §3.12 | `none` | no |
| `outline-width` `-style` `-color` | as the `border-*` equivalents | `0` / `none` / `currentColor` | no |
| `object-fit` | `fill` \| `contain` \| `cover` \| `none` \| `scale-down` | `fill` | no |
| `list-style-type` | `disc` \| `circle` \| `square` \| `decimal` \| `none` | `disc` | **yes** |
| `list-style-position` | `inside` \| `outside` | `outside` | **yes** |
| `transform` | `none` \| a bounded list of `translate()`, `scale()`, `rotate()` | `none` | no |
| `transform-origin` | `<length>` \| `<percentage>` ×2 | `50% 50%` | no |

`transform` is **paint-level only**: it never affects layout, so G2 cannot be disturbed by
it. **Excluded:** `filter`, `backdrop-filter`, `mix-blend-mode`, `clip-path`, 3-D
transforms, animations and transitions.

### 3.9 Tables

| property | admitted values | initial | inh. |
|---|---|---|---|
| `border-spacing` | `<length>` ×2 | `0` | **yes** |
| `vertical-align` (table cells) | `top` \| `middle` \| `bottom` \| `baseline` | `baseline` | no |

**`border-collapse` is absent — borders are always separate.** The collapsing-borders
algorithm is one of the most intricate in CSS (conflict resolution across four edges of
adjacent cells) for a purely visual effect. Column widths arrive pre-measured (§3.13).

**Count: 64 property rows across §3.3–§3.9**, which is the denominator M2's conformance
number is expressed against. Grouped longhands (`margin-top/-right/-bottom/-left`) count as
one row; an implementation must support all four.

### 3.10 Selectors and cascade


Admitted: type, `.class`, `#id`, attribute selectors, state pseudo-classes, the child
combinator `>`, `:is()`/`:where()` over admitted selectors.

**Excluded: the general descendant combinator**, `!important`, and `@import` beyond a
bounded depth.

Resolution order is: UA layer, then author layers in declared order, then source order.
Specificity is retained within a layer but cannot cross one — which is what makes a
rule's effect determinable from a bounded context (G4).

### 3.11 Media queries

`width`, `height`, `orientation`, `prefers-color-scheme`, `prefers-reduced-motion`,
`prefers-contrast`, and bucketed `resolution`. **Bucketing is load-bearing**: media
queries are evaluated client-side and leak nothing to a server, but any *fetch* decision
that varies with client characteristics (a `srcset`-style mechanism) would leak them, so
v1 admits no client-characteristic-dependent fetching.

### 3.12 Static bounds (G3)

Every ceiling below is normative and checked by the parser and the scene-graph validator.
A document exceeding any of them is **refused with a diagnostic**, never truncated or
clamped — a clamped document renders wrongly and silently, which is the failure mode this
project has been bitten by repeatedly.

**Derivation rule.** Each measured ceiling is the corpus **p99, given headroom and rounded
up to a power of two**. p99 rather than max, because a single pathological artifact should
not set a compatibility surface; headroom, because a ceiling that refuses real content is
worse than one that bounds loosely — the purpose is to make resource use *finite*, not to
make it small.

#### The corpus actually used

360 HTML documents and 40 stylesheets from this tree (FreeBSD contrib docs, the Lua
manual, EDK2, generated reports), plus 95 Markdown specs under `docs/`:

| measure | median | p95 | p99 | max |
|---|---|---|---|---|
| document bytes (HTML) | 5,510 | 64,163 | 140,121 | 564,379 |
| elements per document | 84 | 1,407 | 2,669 | 12,594 |
| tree depth ★ | 8 | 34 | 65 | 1,345 ★ |
| stylesheet bytes | — | — | — | 31,024 |
| rules per stylesheet | — | — | — | 266 |
| table columns (Markdown) | 3 | 6 | 12 | 12 |

★ **The depth figures are inflated and must not be read as structural nesting.** The
measuring parser does not implement HTML5's implied end tags, so unclosed `<p>`/`<li>`/`<dt>`
in legacy markup accumulate on its stack — the 1,345 outlier is the Lua manual, not a
document nested 1,345 deep. This profile requires *well-formed* input (§2), where that
inflation cannot occur, so the depth ceiling below is set from the median and p95 with
generous headroom rather than from the contaminated tail.

★★ **This is a convenience corpus, not a representative one.** It is source-tree
documentation, not a sample of the Wikipedia/news/blog content the document lane targets,
and it contains no grids, no `calc()`, and no custom properties at all. These ceilings are
therefore **provisional**, and §8 keeps re-derivation on a representative corpus open. The
derivation *method* above is the durable part; the numbers are the best available now and
are explicitly better than invented ones.

#### The ceilings

| ceiling | value | basis |
|---|---|---|
| total document bytes | **4 MiB** | ~7× observed max, ~30× p99 |
| elements per document | **65,536** | ~5× observed max, ~24× p99 |
| tree depth | **256** | ~7× p95; set off median/p95, see ★ |
| stylesheets per document | **16** | reasoned — no corpus signal |
| `@import` depth | **4** | reasoned |
| bytes per stylesheet | **1 MiB** | ~34× observed max |
| rules per stylesheet | **16,384** | ~60× observed max |
| `calc()` nesting depth | **16** | reasoned — hand-written `calc()` rarely exceeds 3 |
| custom-property substitution depth | **16** | reasoned; cycles are a diagnostic regardless |
| grid tracks per axis (incl. `repeat()` expansion) | **1,024** | reasoned — no corpus signal |
| image dimension (either axis) | **16,384 px** | matches common GPU texture limits |
| **total decoded image bytes** | **256 MiB** | the binding constraint (below) |
| `box-shadow` blur + spread | **256 px** | bounds rasterization cost, not structure |

**Two of these interact deliberately.** A single 16,384 × 16,384 image decodes to about
1 GiB, which the 256 MiB total forbids — so the dimension cap is a cheap early reject on a
header, and the decoded-bytes cap is what actually bounds memory. Stating both is not
redundancy: the first is checkable before allocating anything.

**Why refuse rather than clamp**, restated because it is the rule most likely to be
softened under pressure: a refused document is a visible failure with a diagnostic and a
position, and the reader knows they are not seeing the content. A clamped one renders
plausibly and wrongly, forever, and nobody finds out.

---

### 3.13 Pre-measured content — the general rule

Replaced content must declare intrinsic dimensions (§1.2), and tables must declare
intrinsic column widths. These are the same rule:

> **Content whose sizing would require unbounded measurement must arrive
> already measured.**

Auto table layout is the canonical unbounded measurement: every cell's content must be
measured to derive each column's min-content and max-content width, and the cost scales
with the document rather than with the viewport. Rather than exclude tables — real
reference content depends on them — the measurement moves **offline**, into the
normalizer or any other conforming producer (§6). A conforming table carries, per column,
a declared `min-content` and `max-content` width.

The renderer then performs a **bounded, deterministic distribution** of those declared
widths into the available inline size — the same shape as resolving grid tracks of
`minmax(min-content, max-content)`. This keeps tables responsive: the expensive,
content-dependent half is precomputed, while the viewport-dependent half stays at render
time where it belongs.

Two properties make the precomputation shareable, which is what makes it worth doing:

- **Intrinsic widths are viewport-independent.** They depend only on the content and the
  font metrics, and the font set is shipped and fixed (§1.1). So one measurement serves
  every reader at every window size — the artifact is not specialised to whoever
  converted it.
- **Therefore it deduplicates.** Converted output is content-addressed in Tessera, so a
  page is measured once and every subsequent reader of those bytes gets the result for
  free, offline.

**Cache-key consequence, load-bearing:** because intrinsic widths depend on font metrics
— and, once §3.14 admits native controls, on widget metrics too — the normalizer's output
identity must be keyed by **(source bytes, profile version, font set version, widget set
version)**, not source bytes alone. A font set update silently reusing measurements
taken against the previous metrics would produce tables that are subtly wrong everywhere
and identical to correct ones by hash. The font set version is part of the address.

A table arriving without declared column widths is **refused with a diagnostic** (§3.12),
exactly as an image without intrinsic dimensions is. The renderer contains no fallback
measurement path — a fallback is how the unbounded algorithm creeps back in.

### 3.14 Native widgets — real controls, never their authority

The document worker's output is a Fresco scene graph, and Pergola's widgets *are* Fresco
scene-graph nodes. So a control in a document is not an imitation of the native control,
it **is** the native control: same rendering, same metrics, same theme, same focus and
input behaviour, same semantics. This is a structural consequence of where the seam was
drawn, not a feature bolted on.

It inverts one of the web's oldest defects. Web controls are unstylable, so every design
system rebuilds them out of generic boxes, and every rebuild is subtly wrong — broken
focus handling, broken keyboard interaction, broken assistive-technology semantics,
inconsistent with the rest of the machine. That reimplementation treadmill exists because
the document language and the toolkit have nothing in common. Here they share the scene
graph, so there is no incentive to reimplement and no gap to reimplement across. System
theme, contrast and text-size preferences apply to document controls natively, and G5's
semantic tree gets widget roles for free because Pergola widgets already carry them.

**The boundary, which is the whole of the security argument:**

> A document may instantiate a widget's **appearance and local interaction**.
> It may never instantiate a widget's **authority**.

Local interaction means state that lives and dies inside the document's own view: focus,
hover, text selection, scrolling, expand/collapse, tab strips, a slider that scrolls a
figure. Anything that reaches beyond the view — choosing a file, granting a permission,
spending money, printing, capturing input globally — is **not a widget the document may
have**. Those are brokered: the document declares an intent, and `navigatord` renders the
real control, in chrome space, under the consent rules of the backend spec. A control
that does something beyond local view state is an *app*, and apps cross the consent
boundary by design (atrium-navigator.md §2.2). Blurring this would dissolve the
document/app distinction the entire architecture rests on.

**Spoofing.** Pixel-perfect native widgets make the classic picture-in-picture attack
sharper: a document could paint a convincing address bar, permission prompt, or system
dialog. The mitigation is structural rather than visual — the document paints only inside
its own Limen surface, and **all trusted UI is composited by a different jail into a
subtree the document cannot address**. Consent never happens inside the content area. The
JS-free invariant helps here too: the timing, overlay and fullscreen tricks that make
these attacks reliable on the web need scripting. A per-session visual token in the
chrome, which content has no way to observe or replicate, is a candidate additional
defence and is flagged as unproven rather than assumed.

**Determinism consequence.** Widget metrics come from the toolkit, so they feed intrinsic
sizing exactly as font metrics do (§3.13). The normalizer's output identity therefore
extends to **(source bytes, profile version, font set version, widget set version)** — a
toolkit update that changed a control's metrics while reusing measurements taken against
the old ones would produce the same silent, hash-identical wrongness that the font-set
argument guards against.

## 4. Semantics are an output, not a derivation (G5)

Browsers compute an accessibility tree *from* the DOM plus ARIA patches, which is why it
drifts from what is displayed. Here the document worker emits **one artifact containing
both** the visual scene graph and the semantic tree, produced by the same pass: role,
name, level, reading order, and the relationship to the boxes that present it.

Consequences worth naming:

- Reading order and visual order cannot diverge, because `order` and the descendant-based
  restyling tricks that separate them are not in the profile.
- The semantic tree is **testable headlessly** — a golden file like any other (§7), so
  "did this change break the semantics" is a CI failure rather than an audit finding.
- Assistive technology consumes a first-class artifact rather than reverse-engineering
  one.

---

## 5. What a conforming implementation must report

Two reporting rules, both from repeatedly having been misled by tools that answered
pass/fail:

1. **Ignored input is reported, never silently dropped.** An unlisted property, an
   unknown element, an unadmitted selector — each produces a diagnostic with a source
   position. Silent drop paths are how a document renders subtly wrong forever.
2. **Conformance is a number.** The corpus run reports what fraction of the profile was
   exercised and what fraction matched, not a green tick. A suite that cannot say how
   much it covered has not told you anything.

---

## 6. Legacy content: tolerance, quarantined

Strict parsing appears to contradict the parent spec's claim that the document lane
covers "Wikipedia, news, blogs, papers". It does not, because tolerance moves out of the
renderer into a separate component:

**A normalizer** — jailed, offline, zero capabilities, output content-addressed — accepts
real-world HTML and emits a profile-conformant document, or fails. It may be as lenient
as it likes, because its output is *checkable* against this profile before anything
renders it; the renderer never sees untrusted legacy markup. Because the output is
content-addressed in Tessera, a given page is converted once and thereafter deduplicated
and offline, and conversion cost is amortised across every reader of the same bytes.

This puts the twenty-years-of-error-recovery complexity in a component that can fail
safely, and keeps the renderer — the thing with the layout guarantees — small and strict.
Content that cannot be normalized is the legacy tail, which atrium-navigator.md §6 already
assigns to Servo, server-side, last.

---

## 7. How each guarantee is tested

| | test | shape |
|---|---|---|
| G1 | golden scene graphs | byte-identical, 3 runs × 2 machines (backend spec M0) |
| G2 | **arrival-order permutation** | render with resources delivered in N permuted orders; assert identical final graph **and** that no box occupies two positions across the sequence |
| G3 | bounds + bomb corpus | every ceiling has a fixture that exceeds it; assert refusal with diagnostic, within RCTL bounds |
| G4 | cascade property test | resolution invariant under rule-evaluation order; no admitted stylesheet can produce an effect outside its bounded context |
| G5 | semantic goldens | semantic tree compared as a golden file; reading order asserted against document order |
| G6 | **environment equality** | same document rendered on hosts differing in installed fonts, locale, time zone, hostname → **bit-identical** scene graphs |

G2 and G6 are the two that are worth building the harness around: each converts a
property browsers can only approximate into a hard, mechanically checkable assertion.

---

## 8. Open questions

1. **Web fonts.** Excluded in v1; revisit with a fingerprinting and ingest argument, and
   note that admitting them threatens G2 unless metrics are known before layout.
2. **Justification quality without hyphenation.** `justify` is admitted but reads poorly
   without hyphenation; either restrict it or take on dictionaries.
3. **Whether to admit `position: fixed`** for document headers, given its interaction
   with scrolling and compositing in Fresco.
4. **Re-derive the §3.12 ceilings on a representative corpus.** They are currently set
   from a *convenience* corpus — this source tree's documentation — which contains no
   grids, no `calc()` and no custom properties, and is not the Wikipedia/news/blog content
   the lane targets. The derivation method (p99, headroom, round to a power of two) is
   settled; the numbers are provisional and become a compatibility surface once published.
