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

### 3.2 Layout

| property | admitted values | notes |
|---|---|---|
| `display` | `block`, `inline`, `inline-block`, `flex`, `grid`, `table`*, `none` | *tables arrive **pre-measured** (§3.9); the renderer distributes declared intrinsic widths, it never measures cells |
| `position` | `static`, `relative`, `absolute` | no `fixed`, no `sticky` in v1 (both interact with scroll/compositing) |
| box metrics | `width`, `height`, `min-*`, `max-*`, `aspect-ratio`, `padding-*`, `margin-*`, `border-*` | |
| `overflow` | `visible`, `hidden`, `auto`, `scroll` | |
| flex | `flex-direction`, `flex-wrap`, `justify-content`, `align-items`, `align-self`, `align-content`, `gap`, `flex-grow`, `flex-shrink`, `flex-basis` | **`order` is excluded** — it decouples visual from reading order, which is an accessibility defect, and G5 makes that defect visible rather than ignorable |
| grid | `grid-template-columns`/`-rows` (incl. `fr`, `minmax()`, `repeat()`), `grid-column`/`-row`, `gap`, `grid-auto-flow` | subgrid deferred |
| `z-index` | integer, within an explicitly declared stacking context | no implicit stacking contexts created as a side effect of unrelated properties |

### 3.3 Typography

`font-family` (profile-named stacks only), `font-size`, `font-weight` (numeric, shipped
weights only), `font-style`, `line-height` (unitless preferred), `letter-spacing`,
`word-spacing`, `text-align` (`start`/`end`/`center`/`justify`), `text-indent`,
`text-decoration`, `text-transform`, `white-space`, `overflow-wrap`, `tab-size`,
`font-variant-numeric`.

Excluded: web fonts (v1 — both a fidelity requirement and an ingest/fingerprint surface,
deserving their own argument), `hyphens` (needs dictionaries; deferred).

### 3.4 Paint

`color`, `background-color`, `background-image` (`url()` and `linear-gradient()`),
`background-position`/`-size`/`-repeat`, `border-radius`, `box-shadow` (bounded blur
radius), `outline`, `opacity`, `visibility`, `object-fit`, `list-style-*`,
static 2-D `transform` (`translate`, `scale`, `rotate` — paint-level, does not affect
layout).

Excluded: `filter`, `backdrop-filter`, `mix-blend-mode`, animations, transitions.

### 3.5 Values

Units `px`, `em`, `rem`, `%`, `fr`, `ch`, `vw`, `vh`; `calc()` with bounded nesting
depth; custom properties with cycle detection and a bounded substitution depth; colours
as hex, `rgb()`, `hsl()`, or profile-named.

### 3.6 Selectors and cascade

Admitted: type, `.class`, `#id`, attribute selectors, state pseudo-classes, the child
combinator `>`, `:is()`/`:where()` over admitted selectors.

**Excluded: the general descendant combinator**, `!important`, and `@import` beyond a
bounded depth.

Resolution order is: UA layer, then author layers in declared order, then source order.
Specificity is retained within a layer but cannot cross one — which is what makes a
rule's effect determinable from a bounded context (G4).

### 3.7 Media queries

`width`, `height`, `orientation`, `prefers-color-scheme`, `prefers-reduced-motion`,
`prefers-contrast`, and bucketed `resolution`. **Bucketing is load-bearing**: media
queries are evaluated client-side and leak nothing to a server, but any *fetch* decision
that varies with client characteristics (a `srcset`-style mechanism) would leak them, so
v1 admits no client-characteristic-dependent fetching.

### 3.8 Static bounds (G3)

Normative ceilings, enumerated in the profile and checked by the parser and the scene-graph
validator: maximum tree depth, node count, total decoded image bytes, individual image
dimensions, stylesheet count and size, `calc()`/custom-property substitution depth, grid
track count, and total document bytes. A document exceeding any ceiling is **refused with
a diagnostic**, never truncated or clamped — a clamped document renders wrongly and
silently, which is the failure mode this project has been bitten by repeatedly.

---

### 3.9 Pre-measured content — the general rule

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
— and, once §3.10 admits native controls, on widget metrics too — the normalizer's output
identity must be keyed by **(source bytes, profile version, font set version, widget set
version)**, not source bytes alone. A font set update silently reusing measurements
taken against the previous metrics would produce tables that are subtly wrong everywhere
and identical to correct ones by hash. The font set version is part of the address.

A table arriving without declared column widths is **refused with a diagnostic** (§3.8),
exactly as an image without intrinsic dimensions is. The renderer contains no fallback
measurement path — a fallback is how the unbounded algorithm creeps back in.

### 3.10 Native widgets — real controls, never their authority

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
sizing exactly as font metrics do (§3.9). The normalizer's output identity therefore
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
4. **The exact numeric ceilings** in §3.8 — these should be derived from measurement on
   the corpus, not guessed, and they are a compatibility surface once published.
