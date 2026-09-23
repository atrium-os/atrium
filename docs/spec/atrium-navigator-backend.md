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
`rect`, `run` and `link` lines. Each run carries its source text too, so a reader of a diff can see what
moved.

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
`Navigate`, `Back`, `Close`, `Report` in; `SessionOpened`, `SceneReady`, `Rewound`,
`Closed`, `Expired`, `Blocked`, `NoSuchSession`, `Report` out.

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

---

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
| **M3** | per-document jail + graph validator | worker capabilities == none; validator fuzzed with reached-coverage reported; **per-document jail launch cost measured**, not assumed |
| **M4** | navigation state machine + history in Tessera | back/forward/session-restore driven headlessly |
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

★ **Two renderer tests had been failing for hours** — a stale sample golden and a pinned
`nsg 0.1` header — behind `grep -c "test result: ok"`, which counts the suites that passed
and cannot see one that failed. Counted properly: **505 tests, 0 failures, six crates**.

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
