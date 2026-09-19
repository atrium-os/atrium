# Scene-graph hardening — every producer is untrusted

**Status:** requirements, not built. Companion to
[wire-format.md](wire-format.md) (the format itself) and the navigator specs
([backend](atrium-navigator-backend.md) §4, [legacy web](atrium-navigator-legacy-web.md) §7).

---

## 1. Why now: the threat model changed

The Fresco wire format (0.1) was designed for **cooperating clients** — Atrium apps that
may be buggy but are not trying to break the server. That was a reasonable scope, and the
spec reflects it: it contains no adversarial threat model, and its two mentions of
validation are incidental.

The navigator changes this. The scene graph is now the artifact that crosses *every* jail
boundary, and two of its producers are hostile by assumption:

- **document workers** — zero capabilities, parsing attacker-supplied bytes;
- **legacy-engine jails** — attacker *code* executes there by design.

So the same format now carries output from producers we assume are compromised.

**Decision: there is one profile, and it is the hostile one.** Every producer is
validated identically — a legacy-engine jail, a document worker, and a first-party
Pergola app all traverse the same path, with no trusted-producer fast path anywhere.

The reasoning is not that Pergola is suspected. It is that:

- **Two validation paths means the strict one is the less-tested one.** A branch taken
  only by hostile producers gets exercised only by the fuzz corpus, while the lenient
  branch carries all the real traffic. That is the wrong way round.
- **A trusted-producer fast path is exactly what an attacker reaches for.** Trust here is
  a property of the producer's *identity*, and identity survives compromise — the moment
  any trusted producer is exploited, the fast path is the attack surface, and it is the
  path with the fewest checks.
- **Strictness helps first-party code too.** A Pergola bug that emits a non-canonical or
  out-of-domain record now fails loudly at the boundary instead of producing a scene that
  renders subtly wrong, or that hashes differently from an identical one. A silent accept
  is how these become permanent.
- **The single path is fully covered by one test corpus**, rather than coverage being
  split across a branch nobody runs in production.

The cost is a bounds-check and a 128-byte copy per record, which should be **measured and
reported** rather than assumed either way; the expensive item (H6's cost model) is
required regardless of who produced the graph.

This document therefore proposes normative changes to wire-format.md (§6) rather than
forking the format or adding a profile flag.

The governing precedent is X11: an expressive, extensible display protocol with
server-side state and client-controlled resources, and a correspondingly long security
history. Wayland's principal improvement was *removing expressiveness*. The rule that
follows —

> **Expressiveness is the attack surface. A scene graph must be data, never a program.**

## 2. What the format already gets right

Worth stating so it is not re-litigated or "hardened" into something worse:

- **Fixed-shape records.** No offsets or pointers into a buffer, so the single richest
  parser-bug class (attacker-controlled offsets, as in font and document formats) is
  absent by construction. This is the format's best security property and it was free.
- **Content addressing.** Bulk data identified by hash gives integrity and dedup, and
  makes tampering detectable rather than silent.
- **Retained mode.** The server owns the scene; clients send mutations. There is no
  re-submission of a whole scene to re-validate each frame.
- **Explicit endianness and alignment.** No implementation-defined layout to disagree over.
- **No scripting, no expressions.** The format is already data rather than a program;
  H5 exists to keep it that way.

## 3. Requirements

### H1 — Shared memory: copy once, then never look again

**The highest-priority item in this document.** wire-format.md §4 specifies
single-producer/single-consumer rings with no locks and volatile writes. That is a
*correctness* model for a cooperating writer, not a security model: a hostile client can
mutate a record after the server has read one field and before it reads the next — the
classic double-fetch.

Required:

1. The server **copies each record out of shared memory into private memory exactly
   once**, then validates and acts on the private copy only. No field is ever re-read
   from the shared mapping.
2. Head and tail pointers written by the client are **untrusted input**. The server
   clamps advancement to a bounded step, never trusts `head - tail` as a count without
   clamping, and treats a wild jump as a protocol violation rather than a large batch.
3. **Ring capacity is server-owned**, fixed at setup, and never read from client memory —
   `index = head % capacity` is only safe if `capacity` is ours.

A consumer that satisfies H1 is immune to the entire class; one that does not is
exploitable regardless of every other measure here.

### H2 — Content-addressed blobs: verify on ingest, share on possession

Two distinct requirements, both load-bearing:

1. **The server hashes ingested content itself, always.** Trusting a client's claimed
   hash permits cache poisoning across jail boundaries: upload X claiming the hash of Y,
   and every later client that references Y receives X. There must be no fast path that
   skips this, for any producer.
2. **Cross-client reuse is possession-scoped.** wire-format.md §2.2 says blobs are
   "reusable across clients, sessions, and machines". Against hostile clients that is an
   existence oracle and a read primitive: a client that names a hash it never uploaded
   learns that someone else holds it, and obtains the content. Atrium already settled this
   shape — possession ledgers in aqueduct.md §6.6, dedup domains in tessera-fs.md §20 —
   and the wire format must inherit it rather than restate a weaker rule. Storage-level
   dedup is untouched; only *visibility* is scoped.

### H3 — Canonical encoding, and reject what you do not understand

Content addressing is only sound if each logical value has exactly one encoding;
otherwise the same scene has several hashes (dedup breaks) and two components can
disagree about the same bytes (parser differential).

Required, of every producer:

- **Reserved and padding bytes must be zero**, and a non-zero reserved field is a
  protocol violation. wire-format.md reserves trailing padding for forward compatibility;
  ignoring unknown padding is a smuggling channel, and zeroing costs nothing.
- **Unknown opcodes are rejected, not ignored.**
- Exactly one encoding per value; non-canonical forms are refused.

This is the inverse of the usual "be liberal in what you accept", and deliberately so:
liberal acceptance is how parser differentials become vulnerabilities.

**Forward compatibility survives, by a different mechanism.** wire-format.md §2.6 obtains
it through tolerance — minor versions add opcodes, and a receiver ignores what it does not
recognise. Rejecting unknown opcodes appears to break that promise. It does not, because
the handshake (§8) already negotiates a version and an extension set:

> **Negotiation replaces tolerance.** A producer may send only opcodes inside the
> negotiated version and extension set; the consumer rejects everything outside it.

This is strictly better even ignoring security. Tolerance is implicit, unbounded, and
untestable — you cannot enumerate what a receiver will silently swallow. Negotiation is
explicit, auditable, and produces a hard error at the moment of disagreement rather than a
scene that renders differently on two machines. New opcodes remain usable as soon as both
ends agree; they simply can no longer be *smuggled* past an older one.

**The vendor-reserved range** (wire-format.md §6.1) is gated the same way: usable only
after explicit extension negotiation, never accepted by default. An opcode range whose
semantics vary by vendor is otherwise a permanently unvalidatable hole.

### H4 — Numeric domain

- **Reject NaN, infinities and denormals** in geometry, transforms and colour. These are a
  standard renderer crash and infinite-loop source, and they have no legitimate meaning in
  a layout.
- **Bound coordinate and scale magnitudes** before they reach rasterization, where large
  values become allocation or iteration counts.
- **Prefer fixed-point for document-lane geometry.** IEEE binary32 is specified by
  wire-format.md §2.8, but cross-architecture float reproducibility is hard, and the
  document profile's G1 requires bit-identical scene graphs across machines. Fixed-point
  serves determinism and removes NaN/Inf simultaneously.

### H5 — Data, never a program

- **No external reference that the consumer resolves.** Every resource is either inline or
  a hash the broker already holds. A consumer that fetches a name supplied by untrusted
  content is the XXE/SSRF class, and it hands a zero-capability worker a network primitive
  through its own consumer.
- **Acyclicity is checked, not assumed.** Graph formats with backreferences are a
  reliable infinite-loop DoS; the structure must be a tree or an explicitly verified DAG.
- **Depth and node-count bounds enforced with an explicit stack.** The validator must be
  total: no unbounded recursion, no unbounded allocation, no panic path.

### H6 — Bound cost, not just size

A structurally tiny, fully valid command can be enormously expensive: a large blur radius,
an extreme scale, a pathological clip path, thousands of overlapping translucent layers.
Rings bound message *rate*; they do not bound *work*.

Required: a **rendering cost model** that estimates cost before execution and refuses
above a per-client budget, with per-jail RCTL as the backstop. Fresco must never block all
clients on one client's frame — per-client queues and deadlines, and refusal in preference
to unbounded work.

### H7 — The compositor owns geometry, z-order and input routing

The Limen boundary is where clickjacking lives.

- Surface geometry and stacking are **assigned by the parent/compositor**, never claimed
  by the child. A child that can declare its own size and position can cover chrome.
- Input is routed by **compositor-owned geometry**, never by a client's claim about what
  it contains, and a client receives only the input actually delivered to it.
- A document surface can never overlap trusted chrome (legacy web §10.8, profile §3.14).

### H8 — The server holds every client's scene

Fresco is a shared process holding the scenes of mutually distrusting clients — the same
property that legacy-web §7.3 warns about, and it is inherent to compositors rather than
fixable. Honest mitigations, not a claim of immunity:

- Memory-safe implementation (the language policy already puts userspace in Rust).
- No cross-client references except possession-checked hashes (H2).
- Per-client resource accounting, so one client's consumption cannot starve another.

### H8.1 — "It is jailed" is not a uniform statement

A reasonable objection to all of the above: the scene-graph parser is itself jailed, so
why does its soundness matter? `frescod` is indeed jailed — portcullis.md §1420 lists it
as a jailed system service with **no filesystem and no network**, and that is real
containment which genuinely limits what a compromise can *remove* (H10.1's point about
needing a receiver applies here too, and applies well).

But a jail is a **capability set, not a binary**, and the strength of the containment is
the emptiness of that set:

| | document worker | `navigatord` | `frescod` |
|---|---|---|---|
| filesystem | none | store | none |
| network | none | none (brokered) | none |
| other | — | jail-spawn | **all rendered content**, scanout, input routing |

The document worker's jail is powerful protection precisely because it is empty —
"compromise yields nothing" is an argument from the capability set, not from the word
*jail*. The compositor's set cannot be empty, because the things it holds are the things
that make it a compositor. A `frescod` compromise therefore yields everything on screen
including other applications and trusted chrome, the input stream, and the ability to
draw arbitrary UI anywhere — which is perfect spoofing, and H7's structural anti-spoofing
guarantee is enforced *by* the component that just fell. Likewise a `navigatord`
compromise holds store access and jail-spawn.

> **Parser soundness matters in proportion to the capability set of the process doing the
> parsing.** In a zero-capability worker it barely matters; in the compositor it is
> load-bearing.

**The design move this argues for: parse hostile input where the set is empty.**

```
untrusted bytes → [zero-capability validator jail] → canonical form → minimal reader
```

A validator in its own empty jail can be compromised to no effect. What then reaches
`frescod` and `navigatord` is **canonical, already-validated, fixed-shape records**, so
the parser that remains inside the privileged components is the simplest one that can
exist — bounds-checked reads of fixed-size structures, no variable-length anything, no
attacker-chosen shape. That residual parser is small enough to audit exhaustively, which
is not true of a parser facing raw hostile input.

**Cost, and why it is affordable:** an extra process hop adds a copy and some latency.
Retained mode is what makes this acceptable — validation happens per *mutation*, not per
*frame*, so the cost scales with how much the scene changes rather than with frame rate.
A static document costs nothing per frame, which is the common case.

Worth noting that none of this is work the navigator invents: a compositor serving
mutually distrusting clients needed robustness against malicious clients regardless. The
navigator makes it urgent rather than new.

### H9 — Remote transport does not replace validation

When a scene graph crosses Aqueduct, the **receiving end runs the full validator
regardless of transport authentication**. An authenticated peer may be compromised;
"it came over a trusted channel" is not a validation result.

### H10 — Residual channels, stated not solved

- Shared cache GC, frame-callback timing and completion latency are **cross-client timing
  channels** inherent to a shared compositor.
- The scene graph is a **steganographic channel** out of a compromised worker. Bounding
  its size bounds the bandwidth; nothing practical closes it. But see H10.1 — the
  honest assessment of this one is much weaker than it first appears.

These are accepted residuals. Listing them is what keeps them from being quietly assumed
away.

### H10.1 — Why the steganographic channel is *mostly* harmless here

A covert channel needs a **source with something worth taking** and a **receiver able to
collect it**. In the document lane the architecture largely removes both, and it is worth
recording that rather than carrying a vague concern forward:

- **The source has nothing to leak.** A document worker is zero-capability and handles
  exactly one document. No credentials, no filesystem, no other sites' data, no session.
  The only thing it holds is the document it was handed — which came from the site
  attacking us, and which that site therefore already has. **Compromising it yields
  access to the attacker's own bytes.**
- **The receiver is the screen.** The graph's destination is a compositor and then a
  human. Collecting a payload from rendered output requires a process that can capture the
  screen, which is itself a capability the attacker must already have obtained by some
  other route.

So the naive reading — "data escapes the jail" — does not hold, and the zero-capability
worker design is what makes it not hold.

Four residuals remain, and they are different in kind from exfiltration:

1. **Implantation, not exfiltration — the direction that actually matters.** Scene graphs
   and converted documents are content-addressed and **shared between users** (backend
   §5). A compromised converter can plant attacker-chosen bytes into an artifact that many
   later readers consume. The risk is not that data leaves; it is that hostile data
   *enters* a shared, durable, nominally-trustworthy artifact — which is why H2's
   server-side hashing and possession scoping are load-bearing, and why converted
   artifacts need recorded provenance (legacy web §5.6).
2. **It pre-positions for a future parser bug.** "Harmless unless the parser has a
   vulnerability" is correct today and is precisely the assumption that historically
   fails. A planted artifact that is deduplicated and durable converts a future parser bug
   from a one-shot attack into one with broad reach and persistence. That is an argument
   for H1–H5 being non-negotiable, not for dismissing the channel.
3. **There is not one consumer, there are several.** The graph reaches accessibility
   tooling, indexing, clipboard, print, save-to-file, and Aqueduct streaming. Each is
   another parser with its own bugs, and some may hold more capability than Fresco. "Only
   a parser vulnerability matters" is reassuring for one parser and less so for *n*.
4. **The legacy lane does hold something.** Unlike a document worker, an engine jail may
   hold an authenticated session for its origin. Still scoped to one origin — which
   already has that data — but the "nothing to steal" argument is weaker there.

**And the genuinely harmful case is not steganography at all.** A structurally valid graph
that draws a convincing fake consent dialog or fake chrome needs no covert channel and no
parser bug: its payload is delivered straight to the human. That is handled structurally —
the document paints only inside its Limen surface, and trusted UI is composited by a
different jail (H7) — and it deserves the attention that the covert channel does not.

## 4. What vigil watches: the validator, not the graph

The jail monitor (legacy web §10) earns its keep because the jail **emptied the baseline**:
a zero-capability worker never legitimately calls `open()`, so any attempt is binary,
unambiguous signal. Extending that monitor to scene graphs is tempting and must be done
carefully, because the scene graph has the opposite property.

> **A scene graph is the worker's legitimate output.** There is no empty baseline — the
> baseline is "arbitrary valid content". Inspecting graphs for suspicious *content* is
> anomaly detection over attacker-controlled data with no ground truth, which is precisely
> the false-positive swamp that vigil avoids everywhere else.

So the answer is not "vigil inspects scene graphs". It is:

> **Vigil consumes the validator's verdicts.** The validator is the component with a
> binary answer; the graph is data.

### 4.1 Hard rule: vigil never parses a scene graph

Two reasons, both disqualifying on their own:

1. **A second parser of the same hostile data is a second vulnerability**, in a component
   that legacy web §10.6 requires to have minimal exposure to attacker input.
2. **Two parsers of the same bytes is a parser differential waiting to happen** — the
   exact hazard H3 exists to prevent. If vigil and Fresco ever disagree about what a graph
   says, the disagreement is the bug.

Vigil receives small, structured verdict events. It does not receive graphs.

### 4.2 The verdicts worth reporting

These have the binary quality that makes the syscall monitor good — arguably more so,
since a *correct producer never emits any of them*:

| signal | why it is binary |
|---|---|
| **validator rejection** (H3–H5) | a conforming producer never emits a rejected graph |
| **protocol violation** (H1) — wild head jump, non-zero reserved bytes, out-of-negotiation opcode | a correct implementation cannot produce these |
| **possession violation** (H2) — naming a hash the client neither uploaded nor was served | the exact signature of hash probing |
| **hash mismatch on ingest** (H2) | content that does not match its claimed address is an attack, not an error |
| **off-origin fetch attempt** (legacy web §10.5) | a worker's origin is known; a request outside it is unambiguous |
| **cost-budget refusal** (H6) | not binary — legitimate content can be expensive — so rate-based, report-only |

### 4.3 The real gain is correlation, not more inspection

A central monitor can do something no per-boundary check can: **relate signals across
boundaries**. One validator rejection is noise-adjacent. A worker that produced a
validator rejection, *then* a syscall denial, *then* an off-origin fetch attempt is a
compromise narrative, and no single detector sees it.

That correlation — not deeper inspection of any one channel — is the argument for vigil
spanning the scene-graph boundary at all.

### 4.4 Flooding is a denial of service against the monitor

A compromised worker can emit violations deliberately, to exhaust vigil's append-only
store or to bury one real event under thousands of manufactured ones. Required: bounded
per-client event budgets, with **aggregation into counters past the budget** rather than
either unbounded logging or silent discard — the count must survive even when the detail
does not.

### 4.5 What this does not catch

Unchanged from legacy web §10.4: a graph that is *structurally valid* and *semantically
hostile* passes every check here, because at the boundary it is indistinguishable from
legitimate output.

Of the two forms, **deception is the one worth watching and steganography largely is
not** (H10.1). Vigil should not spend effort hunting covert payloads: the source has
nothing worth taking, the receiver is a screen, and an output-bytes-to-document-size
heuristic would be a false-positive generator of exactly the kind §4 rejects. What it
*should* report is the implantation path — a converter whose artifact is about to be
shared, whose provenance is incomplete, or whose ingest hash did not match.

## 5. Testing

- **Rogue-client harness.** A deliberately hostile client that continuously rewrites ring
  records under the server, jumps head pointers, and claims false hashes — asserting the
  server's behaviour is unaffected. This is the direct test for H1 and H2, and it is
  mechanically checkable rather than a review judgement.
- **Malicious-graph corpus.** One fixture per requirement, each asserted refused — **and
  each asserting the refusal counter moved**, because a validator that cannot fire reports
  zero and reads exactly like a clean system.
- **Canonicalization round-trip.** Parse → re-serialize → byte-identical; catches
  non-canonical acceptance (H3) and is a prerequisite for sound content addressing.
- **Validator fuzzing** with reached-coverage reported, not exec count.
- **Totality property tests.** The validator never panics, never recurses unbounded, and
  never accepts anything exceeding a stated bound.

## 6. Proposed changes to wire-format.md

Folded into 0.2 rather than kept as a separate profile, since a security property that
lives in a companion document is one nobody implements:

1. A **threat-model section** stating that clients may be hostile.
2. §4 gains the **copy-once rule** (H1) and untrusted-pointer handling as normative.
3. §2.2 gains **possession-scoped visibility** and mandatory server-side hashing (H2).
4. §2.6/§3/§9: **forward compatibility moves from tolerance to negotiation** (H3) — zero
   reserved bytes, unknown opcodes rejected, vendor range gated behind explicit extension
   negotiation. This is the most disruptive proposal here, because it changes a stated
   compatibility promise rather than adding a check.
5. A statement that **validation is uniform**: no producer identity grants a fast path.
6. A **numeric-domain section** (H4), including the fixed-point recommendation for the
   document lane.
7. A **cost-model and per-client budget** section (H6).

## 7. Open questions

1. **Fixed-point for the document lane** — whether it can be adopted without a second
   geometry representation in the format, and what precision layout actually needs.
2. **Cost-model accuracy.** An estimator that is wrong in the cheap direction is a DoS;
   one that is wrong in the expensive direction refuses valid content. It needs
   measurement against a real corpus.
