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

**Headless mode** substitutes "serialize the scene graph to OTL on stdout" for "attach
to Fresco". That single substitution is what makes the whole backend testable without a
display server, and it is the only difference between the test configuration and the
shipping one.

---

## 4. The scene graph is the boundary

Fresco is a retained **scene-graph** server and OTL is a serialized scene graph, so the
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

- **Golden scene-graph tests.** Fixture → OTL → compare. Gated on §4.1 determinism.
- **Profile conformance.** A curated corpus, reported as a **number**, not pass/fail: a
  checker with no coverage figure passes vacuously, and "all green" against three
  fixtures has told us nothing here before.
- **Parser fuzzing.** Reuse the two-phase `scripts/core-fuzz.sh` shape for the HTML, CSS
  and OTL-validator parsers. Report **coverage actually reached**, never exec count — a
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
| **M0** | Markdown → OTL, hermetic | byte-identical output, 3 runs × 2 machines |
| **M1** | jailed fetcher | fetcher's filesystem access refused **at the syscall**, asserted by test |
| **M2** | Document Profile v1 (HTML + CSS) | curated corpus with a reported conformance **number** |
| **M3** | per-document jail + graph validator | worker capabilities == none; validator fuzzed with reached-coverage reported; **per-document jail launch cost measured**, not assumed |
| **M4** | navigation state machine + history in Tessera | back/forward/session-restore driven headlessly |
| **M5** | UI attach (Pergola chrome + Limen document surface) | **UI deleted ⇒ suite still green** |
| **M6** | app-launch path (`atrium-app://` → Nomenclator → Opifex → jail) | trial-launch consent gesture; signature verified before launch |

---

## 9. What this reuses

Almost none of this is new infrastructure; it is composition, which is the point of the
thesis. Tessera CAS (store, dedup, offline, integrity) · Portcullis/`jaild` (per-document
and per-app jails) · Fresco + Pergola (render, chrome) · Limen (composition) · Aqueduct
(control plane, remote UI) · Nomenclator (naming) · Opifex + Sigstore (bundle delivery
and trust) · RCTL/memoryd (resource bounds) · Laminar (scheduling, energy attribution).

## 10. Open questions

1. **Per-document jail launch cost.** The one-jail-per-document default is only viable
   if launch is cheap. M3 measures it; if it is not, the fallback is jail reuse *within*
   a top-level site, which weakens the isolation story and must be argued explicitly
   rather than slid into.
2. **Web fonts.** Excluded from Profile v1; they are both a layout-fidelity requirement
   and a fingerprinting/ingest surface, and deserve their own argument.
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
