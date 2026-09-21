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
`atrium-navigator-recording/1`.

A recording is **one document plus a table of what a reader can do to it**:

```json
{ "format": "atrium-navigator-recording/1",
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
