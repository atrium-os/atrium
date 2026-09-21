# The legacy web lane — compatibility without a JavaScript sandbox

**Status:** design, not built. Third of the navigator specs:
[atrium-navigator.md](atrium-navigator.md) (thesis),
[atrium-navigator-backend.md](atrium-navigator-backend.md) (backend/frontend seam),
[atrium-document-profile-v1.md](atrium-document-profile-v1.md) (the document lane).
This document covers the part those three deliberately exclude: the existing web, which
is not going to be rewritten for us.

---

## 1. The premise correction

"Run it server-side" is sometimes heard as "the site must change". It does not. Running
an engine against an unmodified site is a **rendering proxy**: the site sees an ordinary
browser fetching it over HTTP. No cooperation, no modification, no negotiation. This is
proven at scale — Opera Mini served a very large user base for years with a thin client
and a server-side engine, and enterprise browser-isolation products ship the same shape
today for exactly our security reason.

And the second correction, which matters more: **"server-side" may mean localhost.**
Location transparency is already the architecture's position (atrium-navigator.md §3.1) —
the engine runs in a *jail*, and where that jail runs is a deployment knob. On the local
machine you get compatibility with no third party seeing your traffic and no added
latency; on a remote host you get the thin-client and battery cases. One mechanism, two
placements.

## 2. The thesis, applied to its hardest case

> **We never build a JavaScript sandbox. We build a jail around someone else's engine.**

The whole project rests on the claim that V8's in-process sandbox exists only because the
OS never sandboxed processes properly. The legacy lane is where that claim gets tested
rather than asserted. We take an existing engine (Servo, MPL-2.0, already position 5 of
the D6 build order) and treat it as **untrusted software**: Portcullis jail, zero
capabilities, no filesystem, network only through the brokered fetcher, and one output —
a scene graph on a pipe.

An engine zero-day then lands the attacker in a jail with nothing in it. Contrast a
browser, where a renderer compromise lands in a process holding a large, privileged IPC
surface to its parent. The work here is integration, not invention: we are not writing a
JIT, a sandbox, or a DOM.

## 3. "Sites need JS" is four populations, not one

| | population | answer | lane |
|---|---|---|---|
| **1** | documents using JS incidentally (analytics, banners, menus, lazy-load) | content is already in the HTML — the normalizer needs no JS at all | document |
| **2** | SPA-rendered documents (empty shell, JS builds the DOM) | JS at **conversion** time, not view time (§4) | document, after conversion |
| **3** | genuine applications (mail, maps, design tools) | the jailed engine (§2). No way around it and no shame in it | legacy |
| **4** | light necessary interactivity (validation, tabs, sort, autocomplete) | a **total** declarative vocabulary (§5) | document |

Populations 1, 2 and 4 are the majority of what people call "browsing", and none of them
requires script execution *while the user is reading*. That is the leverage.

## 4. JS at conversion time

The normalizer (document profile §6) is already jailed, offline, zero-capability, and its
output is content-addressed. Give it a JS engine and it resolves population 2: run the
page once headlessly, snapshot the resulting DOM, emit a profile-conformant static
document. The reader then gets the fast, zero-authority lane with all six guarantees
intact, and the conversion is amortised across every reader of those bytes.

**Hard rule: never share a conversion made with credentials.** A prerender of a logged-in
page is a render of someone's session. Personalised conversions go in a private or salted
dedup domain (tessera-fs.md §20); only anonymous fetches may produce artifacts in a shared
domain. Getting this backwards turns the dedup win into a data leak.

Honest cost: prerendered artifacts go stale, and staleness is a Nomenclator freshness
question, not a store question.

## 5. A total interaction vocabulary

Population 4 needs *interactivity*, not *Turing-completeness*: show/hide, tabs,
accordions, validation, sort and filter, autocomplete. A bounded declarative vocabulary —
named states and transitions, data bindings, constraint-based validation, an expression
language with no loops and no recursion — covers these while preserving every guarantee in
the document profile: it is deterministic, analysable, and provably terminating.

**The discipline to write down now:** every declarative interaction language in history
has grown toward Turing-completeness under feature pressure. Additions must preserve
totality *by construction*, and "just add a loop" is the change that ends the document
lane's guarantees. If a feature cannot be expressed totally, it belongs to an app, not a
document.

### 5.1 Where JS runs matters more than whether it runs

A concern worth settling before §5.2: does admitting JS anywhere forfeit the benefits that
come from being JS-free? No — because the benefits come from JS not running **in the
reader's session**, and conversion-time execution (§4) does not.

| | JS at view time (a browser) | JS at conversion time (the normalizer) |
|---|---|---|
| fingerprinting | profiles the reader's machine | profiles the *converter's* hermetic environment — a fixed synthetic host. The reader is not present |
| timing side channels | live against the reader | the script is not running while anyone reads |
| credentials | the user's session is in scope | no session; shared conversions are anonymous by rule (§4) |
| output | arbitrary ongoing DOM mutation | a static artifact, validated against the profile before anyone sees it |

So the JS-free-at-view invariant is preserved exactly. The question is never "JS or no JS",
it is "which side of the conversion boundary".

### 5.2 Transcription: what it cannot be

The appealing idea is a jailed **transcriber** that converts a site's JavaScript into
native declarative interactions once per unique script, amortised by content-addressed
dedup. The economics are attractive and the dedup insight is sound. As a general
JS-to-declarative *compiler*, however, it cannot work, and the reasons are structural
rather than engineering effort:

1. **The target is deliberately less expressive.** §5's vocabulary is total — no loops, no
   recursion, provably terminating. JavaScript is Turing-complete. Translating a
   Turing-complete language into a total one is not possible in general; it is a
   computability fact, not a gap to close. A transcriber can only ever succeed on whatever
   subset happens to fall inside the total fragment.
2. **JavaScript's behaviour is not a function of its syntax.** `eval`, `Function()`,
   `Proxy`, getters and setters, prototype mutation, dynamic property access, monkey-patched
   builtins — real-world semantics depend on runtime values. Every static JS analyser is
   approximate for this reason, and minified bundles are the adversarial case.
3. **The artifact is a framework, not logic.** Real sites ship a bundled framework plus app
   code. There is no discrete "this function toggles the menu" to lift out; the behaviour
   emerges from a framework runtime, a component tree, and state.
4. **Equivalence is unverifiable.** A wrong transcription yields a subtly misbehaving page,
   which is worse than an honestly unsupported one. Proving two programs equivalent is not
   generally possible, and per-site validation would destroy the "once per script"
   economics that motivated the idea.
5. **The dedup win lands on the wrong half.** A given minified jQuery is one hash across
   millions of sites — genuinely valuable. But app bundles are per-site and often
   per-deploy (content-hashed filenames, rebuilt continuously), and the app bundle is
   exactly the part that implements the site's behaviour.

### 5.3 What does work: recognise and record, do not translate

Reframed, the idea is valuable — as a **recogniser and a recorder** rather than a compiler.
Three tiers, increasing in cost and decreasing in fidelity guarantee:

**Tier 1 — content extraction.** No JS at view time, content taken from the HTML. Handles
population 1. Already the plan.

**Tier 2 — prerender plus recorded state machine.** The normalizer already runs the page
once (§4). Extend it from capturing the *DOM* to capturing *observable states*: drive the
page — open each menu, each tab, each accordion — and record the resulting DOM diffs as
named states and transitions in the §5 vocabulary. This is black-box: it never needs to
understand the code, only the states it exhibits, which is precisely what our total
vocabulary can express. It covers most of population 4.

*Limits, stated so they are not discovered later:* only finite, enumerable, deterministic
state spaces; nothing data-dependent (search results, infinite scroll, anything
server-fed); exploration is a crawling problem with combinatorial blow-up; and a page may
behave differently under real input than under exploration.

**Tier 3 — known-artifact substitution by hash.** Here the dedup insight pays off
properly. A content hash identifies an exact artifact — this *is* jQuery 3.6.0, this *is* a
known analytics SDK, this *is* a known consent banner — and the response is not to
translate it but to **substitute a known native behaviour or a no-op**. High confidence,
because the hash pins the exact bytes rather than inferring intent.

*Cost:* a maintained recognition list, with the same arms-race and staleness properties as
filter lists; every version bump needs an entry.

**Tier 4 — the jailed engine** (§2), for population 3, if we ship one at all (§5.4).

### 5.4 The engine decision should be empirical, and deferred

Whether to ship a legacy engine at all is a product decision, not a technical one:
without it, genuine web applications simply do not work, and that may be an acceptable
scope for a reading-first system.

**Recommendation: build tiers 1–3 first and defer the engine decision until it can be
measured.** The experiment is concrete — take a corpus of real sites and measure what
fraction is usable under tier 1, then tier 1+2, then tier 1+2+3. That number decides
whether the largest single piece of work in D6 is worth starting, and it is cheap to
obtain relative to the engine itself.

### 5.4.1 The experiment, run — and what it says

§5.4 asked for a concrete number and deferred a decision on it. The number now exists,
from `navigator-prerender` over a 104-document corpus of real sites (85 with script).
It is reported by the converter itself, so it can be re-run rather than believed.

**Tier 2's prerender half completes on 82–84% of scripted documents.** That is the
fraction whose scripts all run without a gap in our host environment — the number §5.4
implicitly asked for, and on its own it reads as an endorsement of tier 2.

**It is the wrong number to decide on.** "Every script ran" is not "the artifact is
better". Measuring the visible text a conversion actually adds:

| outcome of running the scripts | documents |
|---|---|
| more content than the raw HTML | 19 |
| no change | 64 |
| **less** content — tier 1 is better | 2 |

So **tier 1 alone is adequate or better for 66 of 85 scripted documents**, and the median
document gains nothing from tier 2 at all. The two regressions are hydrating apps that
tear down server-rendered content and rebuild it emptier; for those, converting is worse
than not converting, which §5.4 did not anticipate and which the tier model must handle
(see §5.4.2).

**What this says about the engine decision.** The case for tier 4 is population 3 —
genuine applications — and nothing here weakens or strengthens that. What it does settle
is the case for tier 4 *as a way to rescue documents*: it would not. The one document
that loses its content runs all 70 of its scripts **cleanly**; engine fidelity is not its
constraint. What it lacks is cross-origin data our privacy boundary refuses. A better
engine fails identically. **The remaining gap in the reading lane is a data-access
boundary, not an execution one**, and that is a policy question rather than an
engineering one.

*Caveats, because the number will be quoted:* the corpus is 104 documents chosen for
breadth, not sampled from traffic; "usable" here is measured as visible text, so a
conversion that improves layout or ordering without adding words counts as no change; and
the 19 gains are not uniform — one document accounts for 92,432 of the 95,858 characters
added. The verdict split is a heuristic, not a browser diff (§4).

### 5.4.2 Never emit an artifact worse than the input

Because tier 2 is tier 1's DOM plus whatever the scripts changed, both artifacts are in
hand at the end of a conversion and the choice costs one extra serialization. The
converter therefore **measures and does not decide**: it reports the document before and
after, the visible text of each, and the transitions it recorded. A separate policy
chooses, with a threshold an operator sets.

The default keeps tier 2 unless it retained under 80% of the visible text. On this corpus
that demotes exactly one document. The threshold is far from both edges — the
catastrophic case retains 0.346 and the only other loss retains 0.996 — so it is chosen
with margin rather than tuned.

Two exemptions are as load-bearing as the rule: a document with almost no visible text has
lost nothing when a ratio over a handful of characters swings, and a document with no
scripts has no tier 2 to publish.

This is a **floor, not a quality check**. A page that replaces good content with an equal
volume of worse content passes it.

### 5.4.3 The state recorder: JS as an oracle, not a producer

§5.3's tier 2 has a second half — recording observable states — and the measurement above
changes how cheap it is.

The page's own code is the only thing that knows which of its elements do something. So it
is run, and **its output is discarded**: what survives is a description of what each
interactive element *does*, expressed against the tier 1 document. Candidates are not
guessed; they are the handlers the page actually registered. Every probe is reverted, and
the artifact is byte-identical to one produced without exploring — verified against a
control arm, not asserted.

**93% of recorded transitions are a single attribute write.** Over the corpus: 314
transitions across 30 documents, 266 anchored to the tier 1 document, and 291 of them
carrying no content at all — because the content they reveal is *already in the tier 1
document, merely hidden*. The commonest interactive element on the web is a class toggle.

This matters for §5.3's stated limits. The combinatorial blow-up it warns about is real for
*exploration*, but the **recording** is small: for 93% of transitions it is a node path, an
attribute name, and a value, which is already expressible in the §5 vocabulary. Only the
remaining 7% need content carried, and that is bounded (32 effects and 16 KiB per
transition, with overflow counted rather than dropped silently).

**Safety, which is not optional here.** Exploration *simulates a user*, which prerendering
does not, and a simulated click must not be able to act. Four invariants:

1. **No credentials, ever.** Conversions are shared through the CAS store, so they are
   anonymous by construction; there is no session to damage.
2. **The page network is CLOSED during exploration** — not restricted to same-origin GET.
   Destructive GETs exist (`/delete?id=`), and closing the channel costs nothing because
   tier 2 excludes data-dependent states by definition (§5.3).
3. **Navigation is refused** — `location.assign` is a no-op and history cannot leave the
   document.
4. **Exploration is a separate opt-in mode.** Prerendering runs the page's own code;
   exploration acts on the reader's behalf, and the two deserve different permissions.

### 5.4.4 A refuted alternative: protected-subtree execution

Recorded so it is not re-proposed. The idea was to make hydration non-destructive *by
construction*: mark parser-produced nodes un-removable, so a page may add, decorate and
reorder the server's markup but never destroy it.

It works, and it is the wrong answer. It rescues the one regression — but the rescued
content then appears **twice**, because the client re-render is added alongside the
original it meant to replace. And the cost lands on documents that were healthy: one
gained 935 duplicated words, its code samples printed twice. It fires on 12 documents, of
which one had a problem. It does not prevent the failure, it exchanges missing content for
duplicated content. The tier 1 fallback of §5.4.2 is strictly better for the case it was
built to rescue.

### 5.5 WebAssembly is a better cage, not a translation

A recurring hope is that WASM offers a way out. It does not, for a simple factual reason:

- **Pages do not ship a WASM equivalent.** WASM is used by a small minority of sites, for
  compute-heavy work (design tools, games, codecs, emulators). There is no dual-publishing
  convention and no site that offers "the WASM version" of its interactivity.
- **WASM cannot drive a page by itself.** It has no direct DOM access; it interacts through
  imported JavaScript functions. So a WASM-using page runs JS *as well*, never instead.

Compiling JS to WASM ahead of time does not rescue the transcriber either. Such tools
either embed a JS interpreter in the module — leaving the semantics exactly as they were —
or support a subset, which runs into §5.2's limits unchanged. Either way arbitrary code
still executes at view time, which is the thing §5.1 was protecting.

WASM does have two legitimate roles here, and it is worth keeping them distinct:

1. **The secondary lane, already settled** (atrium-navigator.md §3.2): any language →
   WASM-IR → AOT (Cranelift) → native jailed app. This is a portability on-ramp for *new*
   apps whose output is a primary-model native binary. The content-addressed economics
   apply cleanly — a module is AOT-compiled once per hash and the result deduplicates.
2. **Defence in depth inside the legacy jail** (optional): running the engine compiled to
   WASM adds memory safety over the engine's own bugs, at a real performance cost. It is
   complementary to the jail rather than a replacement — the jail bounds OS-level escape,
   WASM bounds memory-level corruption — and it does **not** address view-time side
   channels, because the code still runs on the reader's machine.

The summary worth remembering: WASM changes the *sandbox*, not the *source material*. It
belongs in the same category as the jail, and it does not move anything into the
declarative lane.

### 5.6 Converter integrity

Two risks that arrive with any shared conversion pipeline, both flagged rather than solved:

- **Cloaking.** A site can serve different content to a converter than to a browser.
  Since the conversion is what readers see, the converter is a targetable surface.
- **Shared-artifact poisoning.** A conversion is served to every subsequent reader of
  those source bytes, so a bad conversion has broad blast radius. Conversions should
  record their exact source bytes and environment, and be independently reproducible, so
  a disputed artifact can be re-derived and compared rather than trusted.

## 6. Output is a scene graph, not pixels

**Decision: the legacy engine emits a Fresco scene graph** (Servo display list → scene
graph), not a pixel stream.

Pixels are much cheaper to port and are what isolation products ship, but they discard
everything that makes content usable: selectable and searchable text, accessibility
semantics, resolution independence, theme and text-size adaptation, and cheap
re-composition on scroll. A pixel lane would also make the legacy experience visibly
second-class in a way that pushes users back to a real browser.

Consequences that follow and must be built, not assumed:

- The scene graph from a legacy worker is **untrusted input**, exactly as a document
  worker's is, and passes the same validator with the same hard limits (backend spec §4.2).
  It is a richer structure than a pixel buffer, so the validator matters *more* here.
- Semantics must be carried across the port (the engine's accessibility tree → our
  semantic tree), or the legacy lane loses G5 and we have two different accessibility
  stories.
- A pixel fallback is acceptable as a *porting milestone*, never as the shipping design.

## 7. Isolation and sharing — where the line actually goes

The natural cost objection is that a whole engine per site is heavy. The natural response
is to split the engine: something dangerous per-site, everything else shared. That
instinct is right, but the line is not "security part vs the rest", because almost
everything downstream of parsing is site-tainted: layout operates on the site's DOM,
painting operates on the site's display list, decoding operates on the site's bytes. The
non-tainted remainder is smaller than it looks.

### 7.1 The rule

> **Share immutable data. Never share a live service that touches site-private input.**

Sharing read-only *data* (code pages, the shipped font set) has no channel: nothing
flows back. Sharing a *service* creates one in both directions — the service sees every
client's input, and a bug in it is reachable by every client.

### 7.2 What is safe to share

- **The engine's code pages.** This is ordinary shared-library behaviour and is free.
- **The shipped font data**, mapped read-only and shaped **in-process**. Note the
  distinction: a shared font *service* would see which glyphs each site renders, which is
  a text-content side channel. Share the data, not the server.
- **Content-addressed artifacts derived purely from public bytes** — normalizer output,
  for instance — governed by the existing dedup-domain policy rather than a new mechanism.
- **The compositor.** Worth stating plainly: the sharing you want largely exists already
  and is called Fresco. A compositor's entire job is to serve mutually distrusting
  clients, and Limen already composes surfaces across jail boundaries.

### 7.3 What must never be shared, and the hole each would open

- **A shared image/media decoder.** Decoders are historically the richest source of
  remote-code-execution bugs in any browser. A decoder shared between sites is a confused
  deputy: compromise it from site A and read site B's decoded content. Decoding belongs
  inside the per-site jail.
- **A shared JIT or JS runtime.** JIT bugs yield arbitrary read/write and are the most
  attacked component in any engine. A shared JIT service would be the single highest-value
  target in the system, reachable by every site at once.
- **A shared code or resource cache across sites.** This is an existence oracle: site A
  detects whether site B loaded resource X by timing. We have already settled this
  analysis for Tessera (tessera-fs.md §20) and the same conclusions apply — but with a
  crucial difference, see §7.4.
- **Any single address space holding two sites' data.** Microarchitectural leakage
  (Spectre-class) makes software boundaries inside one address space unreliable. The
  industry spent a decade trying the software way and then moved to process-per-site; we
  should not re-derive that result experimentally.

### 7.4 The legacy lane loses the document lane's structural protections

This is the point most easily missed. Much of the document lane's safety comes from the
JS-free invariant: no scripted timer and no scripted fetch means the classic cache-probing
and fingerprinting primitives have no vehicle (profile §1.1). **In the legacy lane,
attacker-controlled code runs.** Every one of those mitigations is gone, and timing
oracles, fingerprinting and cache probing are live again.

So the legacy lane must be reasoned about as if it were a browser, because for these
purposes it is one. It inherits none of the document lane's guarantees, which is precisely
why the lane must be visible to the user (§8) and why sharing decisions here are stricter
than they would be in the document lane.

### 7.5 What actually solves the cost problem: fork before taint

The real cost of a per-site engine is startup — mapping the binary, initialising the
runtime — not steady-state memory, which read-only page sharing already handles.

**Zygote model:** keep one fully initialised engine process that has never seen any site
content, and fork it per site at navigation. All initialisation work and all read-only
pages are shared through copy-on-write, and cross-site exposure is exactly zero because
the fork happens *before* any site data exists in the process. This is what Chrome and
Android do, and it gets the user's sharing benefit with none of §7.3's holes.

It composes well here: Tessera's CAS already shares the engine's on-disk image, and jails
are the unit we fork into.

### 7.6 The isolation unit is the origin

Browsers isolate by site (eTLD+1) rather than origin, a compatibility concession to
existing content. **We have no such debt, so the unit is the origin** — strictly stronger.
Cross-origin embedding, the out-of-process-iframe problem that took browsers years to
retrofit, is native here: Limen composes separate jailed surfaces by construction.

With the zygote model the process multiplication that origin-granularity implies is
affordable. If measurement says otherwise, the fallback is site-granularity, argued
explicitly and written down — never slid in.

### 7.7 The fetcher needs per-origin credential state

Our current design has one brokered fetcher holding the only network capability (backend
spec §2). It sees every origin's traffic, which makes it a cross-site channel and a
confused deputy if it ever holds several origins' credentials at once. Credential and
connection state must be partitioned per origin, and a fetcher instance should be
ephemeral per request where that is affordable. Flagged here as a gap in the backend spec
rather than left implied.

## 8. Two lanes, and the lane is visible

The user-facing consequence is a two-lane world, and the lane must be legible rather than
hidden: fast, safe and zero-authority by default; compatibility lane with an indicator and
a different trust posture.

Reader mode is the precedent — browsers already ship "strip the cruft and show me the
content", and users like it. This architecture inverts the priority: **reader mode is the
default and the fast path, and the full engine is the fallback.**

## 9. Honest costs

- A full engine port (Servo → Fresco scene graph, jailed, brokered network) is the largest
  single piece of work in D6, which is why it is last in the build order.
- A local engine per origin is heavy — though no heavier than what a browser already does
  on the same page, and it is not the default path.
- Prerendered artifacts go stale.
- The legacy lane offers none of the document lane's six guarantees, and saying otherwise
  would be the most damaging overclaim available in this design.

## 10. The jail monitor — the jail refuses, the monitor reports

**Decision: ship a jailed JS engine (§2), and pair every jail with a monitor that reports
attempts to reach what the jail forbids.** Confinement and detection are complementary:
the jail is why the attempt fails, the monitor is how anyone learns it happened.

This generalises beyond the browser to every Portcullis jail, and if adopted
platform-wide it should graduate into its own spec rather than living here.

### 10.1 Why the signal-to-noise is exceptional here

Host intrusion detection usually drowns in false positives, because normal software
legitimately does a wide variety of things and the baseline is noisy. That objection does
not apply to these workers, and the reason is worth stating precisely:

> **A zero-capability document worker has a near-empty legitimate syscall profile** —
> read a pipe, write a pipe, map anonymous memory, exit. Anything outside it is a bug or
> a compromise.

There is no third explanation to sift for. The alert budget is tiny and every alert means
something, which is the opposite of the usual IDS economics. **The jail is what makes the
detector good**: by emptying the baseline, confinement converts detection from a
statistical problem into a binary one.

This also sets the implementation style: **deterministic rules, not a learned model.**
A rule like "this worker attempted `open()`" is reasonable about, testable in CI, and
cannot drift. An anomaly model is none of those.

### 10.2 The refusal path reports; nothing infers

The monitor should not be an external observer inferring intent from the outside. The
**denial points themselves emit structured events** — the jail check, the capability
check, the broker's rejection — because a refusal that is not reported reads exactly like
a refusal that never happened. This project has been bitten repeatedly by silent-drop
paths, and an unreported `EPERM` is one.

### 10.3 Two rules this project has already paid for

1. **On by default, never a debug switch.** An audit you enable by hand is off at the
   moment it matters — established while chasing snapshot damage, where every run that
   produced damage had the audits off because nothing turned them on.
2. **The monitor must be provably able to fire.** It needs a fault-injection path that
   makes a worker attempt a forbidden access, with a test asserting the monitor reported
   it. Shipped with the monitor, not added later. A detector that cannot fire reports
   zero and reads exactly like a clean system — which is not hypothetical: a snapshot
   detector in this tree returned "clean" across 122,707 checks while being structurally
   incapable of failing.

### 10.4 What it catches — and what it does not

**Caught:** a compromised worker probing for escape. Opening files, opening sockets,
enumerating processes, touching devices, attempting `execve` — the reconnaissance that
follows almost every exploit, and the first thing an attacker does after landing.

**Not caught, and this must not be overclaimed:**

- **Compromise that stays in-lane.** An exploit achieving its goal entirely through
  *allowed* channels never trips a denial. The monitor detects escape *attempts*, not
  compromise.
- **Side channels.** Spectre-class leakage produces no syscall and no denial.

So the honest claim is "we detect attempts to leave the box", not "we detect intrusion".

### 10.5 The allowed channels are the real exfiltration risk

Because §10.4 leaves in-lane compromise invisible, the allowed channels deserve the
scrutiny the forbidden ones get automatically:

- **The scene-graph pipe.** A compromised worker can encode data into a structurally
  valid scene graph. The validator (backend spec §4.2) bounds its *size and shape*, not
  its *content*, and no practical mechanism closes a steganographic channel here.
- **The brokered fetcher.** Network access is the one genuinely useful exfiltration path,
  which is why the per-origin partitioning of §7.7 is load-bearing rather than tidy: a
  worker confined to fetching from its own origin can leak the site's data *to that
  site*, which already had it. **A fetch request outside the worker's origin is exactly
  the alert worth raising**, and it is a rule the monitor can state exactly.

### 10.6 The monitor is itself a shared service

§7.1's rule applies to the monitor: it observes every jail, so it is a cross-site
aggregation point and a confused-deputy candidate. Consequences:

- It must not be reachable *from* the jails it watches.
- Its input is attacker-influenced, so its own parsing must be minimal and its store
  append-only.
- **Its contents are browsing history.** Denial records name origins and times, which is
  among the most sensitive data on the machine. Reports stay local by default; nothing
  leaves the machine without an explicit, per-destination decision by the user.

### 10.7 Response policy: report always, kill carefully

Killing a jail on its first denial is tempting and wrong as a default — real libraries
probe for `/dev/urandom`, locale files and `/proc` during start-up, so first-denial
termination converts a benign probe into a denial-of-service, and a site could trigger it
deliberately. The staged policy:

1. **Always report.** No denial is silent.
2. **Baseline, then enforce.** Learn each worker class's legitimate profile with the
   monitor in report-only mode, then promote stable rules to terminating ones.
3. **Terminate on exploitation patterns**, not on single probes — repeated distinct
   denials, denials after a parse of hostile input, attempts at `execve` or device nodes.

Because the baseline is near-empty (§10.1), this staging converges quickly rather than
becoming a permanent tuning exercise.

### 10.8 A user-facing signal worth having

The empty baseline makes an unusually concrete statement possible: *this site's renderer
tried to read your files*. That is a far better security signal than the padlock the web
offers, precisely because it reports an observed event rather than a property of the
transport. The discipline is not to spend it — surfacing routine probes as alarms trains
users to dismiss the one that matters.

**Naming:** the Roman night watch were the *vigiles*, which fits both the convention and
the job; `vigild` is offered as a candidate, not a decision.

## 11. Engine selection and execution mode

### 11.1 Two decisions, not one

"Servo or build our own" merges two choices. **Servo is a browser engine, not a JS
engine** — it brings SpiderMonkey with it. And the JS engine is not independently
selectable in practice, because the hard part is not the VM, it is binding the VM to a
DOM. Choose the browser engine; the JS engine arrives attached.

### 11.2 We do not write an engine *now*

**Decision: adopt a mature engine for the legacy lane today. This is a sequencing
decision, not a permanent verdict** — see §11.8, which keeps a memory-safe engine open as
the architecturally correct end state.

Reasons it is not the thing to build first:

- ECMAScript conformance is measured against test262's tens of thousands of tests. A
  credible engine is many engineer-years of the most security-sensitive code in the
  project, and it is not currently the bottleneck — tiers 1–3 are (§5.3).
- Nothing about the engine choice is decidable before §5.4's corpus measurement. Building
  an engine to serve a lane whose size is unmeasured is the same error as porting Servo
  before measuring (§11.4).
- A fresh engine is less *compatible* than a mature one for years, and in the legacy lane
  compatibility is the entire purpose — an incompatible legacy engine has no reason to
  exist.

Note what is **not** among the reasons: that it is hard or large. This project builds
filesystems, schedulers and compositors on the argument that doing it right matters more,
and "too much effort" would be inconsistent with that charter. The argument here is about
*order*, and about the legacy lane specifically being a fallback rather than the main
path.

### 11.3 The shortlist, and what the licensing policy decides

| candidate | licence | verdict |
|---|---|---|
| **WebKit / JavaScriptCore** | LGPL | **Hard reject.** LICENSING-POLICY.md admits no LGPL anywhere in the runtime stack |
| **Chromium / Blink / V8** | BSD-3 | Licence fine; rejected on architecture — colossal, own build system, and it assumes its own process and sandbox model, which is precisely what we are replacing |
| **Gecko** | MPL-2.0 | Enormous, never designed for embedding |
| **Servo** (+ SpiderMonkey) | MPL-2.0 | **Selected.** Rust, embeddable by design, actively maintained. MPL-2.0 sits in the policy's "evaluate per-component" bucket, and toolkit-backends.md already set that precedent |

Servo was already position 5 of the D6 build order; this confirms it rather than changing
it. Its weaker web-compatibility relative to Blink is tolerable *because* tiers 1–3 carry
the reading cases (§5.3) — it is a fallback, not the main path.

### 11.4 Sequencing: measure before porting the expensive thing

**Do not start with Servo.** Tier 2 — prerendering SPA content in the normalizer — does
not obviously need a browser engine. **A JS engine plus a minimal DOM** is dramatically
cheaper, and it is precisely the instrument that produces §5.4's corpus number — the
measurement that decides whether the Servo port is worth starting at all. Building the
expensive thing first, to discover whether it was needed, is the wrong order.

**Decision: the instrument is Boa plus a minimal DOM.** Unlicense/MIT (allowed outright,
no per-component evaluation), ~95.5% test262, a memory-safe object model, and its one
weakness — speed — is irrelevant to a converter that runs offline.

**Build it with a thin engine boundary.** The minimal DOM is the bulk of the work and is
engine-agnostic; keeping the embedding surface narrow means the same instrument can later
be re-run on a different engine. That is not hypothetical tidiness — it makes the
instrument double as the evaluation harness for §11.8's trigger 1, measuring a candidate
engine against *our* corpus rather than against a published score.

**Honest limit, to be stated with the result:** a DOM without layout fails on content
calling `getBoundingClientRect`, `offsetWidth`, or the observer APIs — the known ceiling
of this approach. The number it yields is therefore a **lower bound** on what tiers 1–3
can cover, and must be reported as one rather than as the answer.

### 11.5 Execution mode: JIT off, AOT to bytecode

**Decision: JIT disabled by default; ahead-of-time compilation to bytecode.**

JIT is the most exploited component of any engine, which is why hardened modes in shipping
products disable it. SpiderMonkey can run interpreter-only; QuickJS has no JIT at all.

**What AOT buys, and what it does not.** It does *not* buy JIT-level performance: JIT's
advantage comes from speculating on observed runtime types, which by definition is not
available ahead of time, so AOT-compiled JS lands around good-interpreter level. What it
buys is the removal of **runtime code generation** — no writable-executable mappings in
the process running hostile code, no JIT spraying, and none of the optimizing-compiler
type-confusion bugs that are the highest-value targets in an engine.

**The realistic form is AOT to bytecode** (the Hermes/QuickJS model), not AOT to native.
It preserves JS semantics exactly, eliminates runtime codegen, starts fast, and produces a
compact content-addressable artifact. Native AOT for JavaScript does not reach JIT
performance anyway, so it pays a large complexity cost for no benefit we need.

**AOT is not a privilege granted to trusted origins.** That framing was considered and
rejected:

1. It reintroduces the trusted fast path that the scene-graph hardening spec eliminated —
   trust-by-identity survives compromise, and the privileged path is what an attacker
   reaches for.
2. Origin trust does not track the risk. A trusted origin is compromised by a CDN
   injection, a supply-chain package or an XSS: **trust attaches to the origin, while the
   risk attaches to the code.**
3. AOT needs no trust. The artifact is produced by *our* compiler in *our* jail, so its
   safety rests on our compiler being sound, not on who authored the source.

**The axis that does make sense is economic: compile by hash popularity, not by origin.**
A framework identified by content hash compiles once and deduplicates across every site
and user — §5.3's tier-3 insight applied to compilation. Per-deploy application bundles
are poor candidates because compiling them is economically pointless, not because they are
untrusted.

**Requirements inherited from elsewhere in this design:**

- The compiler consumes hostile input, so it runs in an **empty jail** (hardening H8.1:
  parse where the capability set is empty), offline and separate from the executing jail.
- Its output is a **content-addressed artifact shared between users**, which is the
  implantation path (hardening H10.1), not the exfiltration one. AOT artifacts therefore
  need the discipline of §5.6: **deterministic compilation**, so any artifact can be
  independently re-derived and compared rather than trusted.

**Honest limit:** this removes the code-generation class, not the engine's attack surface.
GC and builtin bugs are untouched — see §11.6. The jail remains the primary boundary; this
is defence in depth, on the same reasoning as hardening H8.1.

### 11.6 Memory management: don't reclaim, rather than don't collect

**Decision: non-reclaiming allocation in ephemeral workers; conventional GC only where
lifetime demands it.**

The garbage collector is, with the optimizing compiler, one of the two largest sources of
exploitable bugs in any engine. It cannot be removed by changing target language: JS
*semantics* require garbage collection — unbounded object lifetimes, cycles, closures, and
no ownership information anywhere — so any target must implement them (§11.7).

But **GC bugs are a consequence of reclaiming memory.** Where a worker is short-lived and
bounded — a document render, a prerender conversion — it need not reclaim at all:

> Make the worker ephemeral, and give the engine an allocator whose `free` is a no-op.

QuickJS already accepts a custom allocator, so this is configuration, not a compiler
project. What it buys:

- **Use-after-free stops being exploitable.** The primitive is free-then-reallocate with
  attacker-controlled data of a different type; with no reuse, a dangling pointer still
  refers to the intact original object. The bug may remain; the exploitation path does
  not.
- **Collector bugs cease to exist** rather than being mitigated — no mark/sweep races, no
  compaction pointer fixups, no premature collection from a refcount error, because
  nothing is ever collected.
- It introduces **no new code of our own**, which was the decisive objection to §11.7's
  alternatives.

Costs and limits, stated so this is not oversold:

- **Buffer overflows are untouched.** Linear overwrites within and across arena objects
  work exactly as before. This removes one large class, not memory unsafety.
- **Memory grows monotonically**, bounded only by RCTL and by process exit. Fine for a
  document parse; heavier for a large prerender, which is offline and amortised.
- **It does not extend to long-running apps.** A tier-4 application open for hours needs a
  real collector and keeps that surface.
- **Marginal value scales with the capability set** (hardening H8.1, H10.1). In a
  zero-capability worker a use-after-free yields control of a process holding nothing —
  already the assumed state. It is worth materially more in the legacy engine jail, which
  may hold an authenticated session, and which is also where memory pressure is highest.

### 11.7 Rejected: compiling JS to native, WASM, or a custom bytecode

Recorded with reasons, because a rejected option without its reasoning gets re-proposed.

The motivation is sound — removing whole vulnerability classes, GC especially — but
changing the *target* **relocates the attack surface rather than reducing it**. A JS
engine's exploitable surface is mostly its *semantics*, not its execution strategy: the
object model (property lookup, hidden classes, prototype chains, getters, Proxy), the
collector, and the builtins (Array, TypedArray, RegExp — itself a compiler, JSON, String).
Only codegen is about execution strategy, and §11.5 already removes that slice.

Compiling to another target cannot delete semantics; it must implement them, and both ways
of doing so are worse:

1. **The artifact embeds a JS runtime** (the Javy / QuickJS-in-WASM shape) — the entire
   runtime, builtin and collector surface is still present, merely recompiled.
2. **The compiler inlines the semantics** into generated code — prototype-chain and
   coercion bugs now live in *our* compiler's output rather than in an engine that has
   absorbed two decades of adversarial attention. Strictly worse.

Target-specific notes:

- **Native.** Production systems that AOT-compile JS to native require *typed subsets*
  (Static Hermes needs annotations; Porffor supports a subset). Unannotated web JavaScript
  is not that language — the same dynamism wall as §5.2.
- **WASM.** The one target that buys something real: type-checked, control-flow integrity
  by construction, memory-safe within its linear memory, so a compromised runtime corrupts
  only its own memory. But it is **largely redundant given an already-empty jail** — the
  jail assumes the worker is fully compromised, and corrupting a zero-capability worker
  yields control of a process with nothing in it, which was the assumed state already. The
  containment lands where it is least needed, at the cost of double interpretation. Where
  capability sets are *not* empty (the compositor, `navigatord`), no JS runs.
- **A custom bytecode** is strictly worse than WASM: no ecosystem scrutiny, no formal
  semantics, and our own verifier bugs to discover.

**What does pay off is not an execution format at all.** §5.3's tiers 2 and 3 convert JS
into *declarative states* and hash-identified substitutions — removing execution rather
than relocating it. That is where effort aimed at "getting rid of JS" belongs.

**One route is not rejected, only deferred:** changing the *pointer discipline* rather
than the target language. See §11.8.

### 11.8 Deferred, not rejected: a memory-safe engine

Of the five collector bug classes in §11.6, four — missed roots and write barriers, type
confusion while tracing, compaction pointer fixups, refcount errors — are all the same
underlying thing: **references to heap objects that are not correctly tracked.** That
suggests attacking the reference representation itself, and the suggestion is sound.

The mechanism is *not* Rust references: the borrow checker handles static lifetimes and
cannot express "this object is live because a collector says so". The mechanism is the
Rust idiom for graph structures — **arena allocation with generational indices in place of
pointers** — plus type-system-enforced rooting, of which `gc-arena`'s branded lifetimes
(a compile error to hold a GC pointer across a collection point) is the state of the art.

Mapped onto the four classes:

| class | effect of arena + generational indices |
|---|---|
| compaction fixup | **eliminated** — no pointers to fix up; data moves, indices stay valid |
| missed root | **downgraded from security to availability** — a freed-but-indexed object is caught by the generation counter, producing a clean error rather than a use-after-free |
| tracing type confusion | largely eliminated — typed arenas make layout statically known |
| refcount errors | eliminated as a *manual* class — refcounting becomes compiler-managed |

It would additionally close the **buffer-overflow class that §11.6 explicitly cannot**, so
on memory safety this is strictly better than the non-reclaiming arena, not merely
different. And it reclaims, which non-reclaiming by definition does not — so it is the
**only** approach that helps long-running tier-4 apps.

**Why it is deferred rather than adopted:** it cannot be retrofitted. SpiderMonkey's and
QuickJS's object models *are* raw pointers and their own collectors; changing the pointer
discipline is rewriting the core.

**The candidate landscape (surveyed 2026-09-19).** Boa is not the only option, and an
earlier draft of this section badly understated it:

| engine | language | licence | test262 | notes |
|---|---|---|---|---|
| **Boa** | Rust | **Unlicense / MIT** | **~95.5%** (≈51k/53k), 4th on test262.fyi, above JavaScriptCore | `boa_gc` tracing collector. Self-described "experimental". Slower than JIT engines |
| **Nova** | Rust | MPL-2.0 | ~80%; `nova_vm` 1.0.0 in March 2026 | **The architectural match**: data-oriented design, normal Rust enums carrying on-stack data or a 32-bit handle into homogeneous arenas, hot/cold split. Safepoint GC built on reborrowing — type-enforced rooting, the idea above applied to a whole engine. Explicitly "not fast"; gaps include sparse arrays, RegExp lookbehind, WASM |
| **brimstone** | Rust | was copyleft, relicensed — **must be verified** | "effectively feature complete"; ~2× Boa's speed | **Compacting collector written in deliberately unsafe Rust**; its author states moving it to safe Rust is impractical |
| **Kiesel** | Zig | — | — | Zig is not memory-safe in the sense this section requires |

**The evaluation criterion is not the implementation language.** brimstone is the
cautionary case: the fastest of the Rust engines, written in Rust, and yet its collector —
exactly the component this section is about — is deliberately unsafe by design. "Written
in Rust" does not imply the object model and collector are memory-safe. The question to
ask of any candidate is **how much `unsafe` lives in the collector and object model, and
whether it is concentrated and auditable** or spread through the hot path.

**Licensing separates them more sharply than conformance does.** Boa's Unlicense/MIT sits
in LICENSING-POLICY.md's allowed-outright column — the only candidate that needs no
per-component evaluation. Nova and Servo are both MPL-2.0, the "evaluate per-component"
bucket. brimstone's relicensing must be checked before it is considered at all.

**Corrected assessment:** conformance is no longer Boa's blocker — at ~95.5% it is ahead
of a production browser engine. Its gap is *performance*, and §11.4's proving ground is
precisely where performance does not matter. Hence the instrument decision there.

### 11.8.2 Trigger 2 was the wrong measurement

Trigger 2 originally read "the corpus measurement showing tier 4 is material rather than
marginal", where the corpus number is the fraction of *pages* convertible to tiers 1–3.
That metric is wrong, and would have been wrong in a way that hid the answer.

**Page count and user stakes are different distributions.** Somebody may read a hundred
articles and use three applications: the articles dominate any page-weighted number while
the three applications hold the hours and all of the consequences. "95% of pages convert"
can be simultaneously true and completely misleading about whether tier 4 matters. Basing
an engineer-year decision on it would be a dominant-variable error.

The sharpest illustration: **webmail is the root of trust for every other account**, since
password resets land there. "Gmail is in tier 4" is not one application in a weak lane —
it is the recovery path for everything else in the lane with the weakest memory-safety
story. No page-weighted percentage surfaces that.

**Replacement metric: an enumerated list, not a percentage.** For each high-stakes task a
real user performs, ask: does an open protocol exist, could a native jailed app serve it,
or is tier 4 the only path?

| task | open protocol? | native path? | tier 4 required? |
|---|---|---|---|
| mail, calendar, contacts | IMAP / JMAP / SMTP / CalDAV | yes | **no** |
| chat | XMPP / Matrix | yes | **no** |
| banking portals | none | no | **yes** |
| collaborative documents, design tools | none | no | **yes** |
| proprietary SaaS | none | usually no | **yes** |

Two conclusions follow, and they point in opposite directions:

- **"Everyone uses webmail" argues for a native mail client, not a better browser engine.**
  Mail has open protocols and is among the best candidates for exactly what the thesis
  prescribes. Where the highest-stakes item in tier 4 has a native path, the right response
  is to move it *out* of tier 4 rather than to harden tier 4 around it.
- **The residue is what decides the engine question.** Banking is the sharpest case: high
  stakes, no open protocol, and one cannot ask a bank to ship an Atrium app. If that list
  is substantial, tier 4 is load-bearing and §11.9's gap is a principal exposure rather
  than a footnote.

This list is obtainable in days and is far more decision-relevant than a corpus
percentage. The corpus number remains useful for a different question — whether the
*document* lane covers ordinary reading — and should not be retired, only stopped from
answering a question it cannot.

### 11.8.1 Why Boa for the instrument when Nova has the better architecture

Both statements hold, because they answer different questions, and the apparent conflict
dissolves in three steps:

1. **The instrument's job is measurement, not production.** What matters is whether it can
   actually run real pages' JS, what it costs to integrate, and its licence. Performance
   and long-term architecture are close to irrelevant for an offline converter that exists
   to produce one number.
2. **An instrument must not confound what it measures.** This is decisive. At ~80%
   conformance with known gaps in sparse arrays and RegExp lookbehind — both ordinary in
   real-world code — Nova would fail to convert pages for reasons that have nothing to do
   with the pages. We could not then distinguish "this content genuinely resists tier-2
   conversion" from "our tool is missing a feature", and §5.4's corpus number would come
   out systematically pessimistic. We would then decide whether to undertake the single
   largest piece of work in D6 on bad data.
3. **Nova's architectural advantage lands in a lane the instrument is not in.** §11.8's
   case — arena plus generational indices, type-enforced rooting — buys most where memory
   must be *reclaimed*, i.e. long-running tier-4 apps. The instrument is ephemeral and
   offline, so §11.6's non-reclaiming arena already removes the same bug classes there for
   free. **Nova's edge is precisely where non-reclaiming fails, and the instrument is
   precisely where it does not.**

So: Boa now, because the measurement must be trustworthy; Nova watched, because it is
building the right thing for the lane where our cheap trick runs out. The thin engine
boundary in §11.4 is what keeps that from being a fork in the road — re-running the same
instrument on Nova is how trigger 1 gets measured when the time comes.

**The incremental path that makes this tractable.** An immature engine does not have to
start in the hardest lane. **Tier 2 conversion (§4) is the ideal proving ground:** it runs
offline, so performance is nearly irrelevant; it is retryable; and a failure degrades to
"convert with the mature engine instead" rather than breaking someone's browsing. The bar
there is *conformant enough to prerender real pages*, which is dramatically lower than
*run the interactive web*. A memory-safe engine can therefore be adopted or grown in a
place where its weaknesses are cheap, and extended toward tier 4 only as it earns it.

**Re-evaluation triggers**, so that "keep it in mind" is actionable rather than a sentiment:

1. **Boa's conformance against our own tier-2 corpus** reaching the level where it converts
   pages the mature engine converts. Measured on our corpus, not on a published score.
2. **The no-native-path task list (§11.8.2) coming back substantial**, especially if it
   includes high-stakes tasks. This replaces an earlier, wrong formulation of this
   trigger — see below.
3. **A vulnerability class in the adopted engine that the jail does not adequately
   contain** — i.e. one that reaches past a zero-capability worker. By H8.1's reasoning
   that is the point at which engine hardening stops being low-value.
4. **The document lane maturing** to where the engine becomes the weakest link in the
   system rather than one risk among many.

### 11.9 The tier-4 gap, named

Tier 4 is the one lane where almost none of this design's memory-safety work applies, and
that should be stated plainly rather than discovered later.

A full browser engine means Servo, Servo means SpiderMonkey, and SpiderMonkey has exactly
the architecture §11.8 argues against: raw pointers, a tracing collector with rooting
obligations spread through the mutator, and a JIT. Of our mitigations:

| mitigation | tier 2 | tier 4 |
|---|---|---|
| jail (zero-capability worker) | yes | yes — but the jail holds a session (§7.4) |
| JIT off, AOT bytecode (§11.5) | yes | yes |
| non-reclaiming arena (§11.6) | yes | **no — the lane is long-running by definition** |
| memory-safe object model (§11.8) | yes, via Boa | **no** |

Swapping the engine does not rescue it: neither Boa nor Nova is a browser engine, and
replacing SpiderMonkey *inside* Servo is not a drop-in — Servo's DOM objects are traced by
SpiderMonkey's collector and its rooting machinery is SpiderMonkey-specific.

### 11.9.1 What genuinely helps

1. **Session recycling.** The obstacle to non-reclaiming is lifetime, not workload — so
   shorten the lifetime. Restart the engine jail on an interval or a memory threshold and
   reload the page, converting "long-running" into a series of ephemeral sessions and
   making §11.6's non-reclaiming arena viable in tier 4 after all. Browsers already
   discard and reload tabs under memory pressure, and web applications are consequently
   reload-tolerant, because browsers reload them constantly. *Costs:* client-side state
   that was never persisted is lost, so recycling must prefer idle moments and never
   interrupt interaction; and the interval trades user disruption against exposure window.
2. **JIT off by default** (§11.5), which removes the largest single class that does still
   apply here.
3. **Per-origin jails with short lives** — the exposure is bounded to one origin's session,
   which is the data that origin already holds (§7.4).

### 11.9.2 Binding a memory-safe engine into Servo — investigated 2026-09-19

The eventual answer is **a memory-safe JS engine bound into Servo**: a new binding and
rooting layer between two existing Rust codebases, rather than a new engine. Findings from
looking at what that actually entails:

**There is an official upstream initiative, and it is low priority.** Servo has a stated
"bring your own JS engine" goal — a Web-IDL-based interface any bindings layer could
implement, explicitly contemplating V8 and Wasm runtimes as well. It is described as
long-term and low-priority. So this is a direction upstream *wants*, meaning the work is
contributable rather than a fork, but nobody is driving it. We would be.

**Four categories of coupling**, per Servo's own report:

1. Low-level `js::jsapi` calls "peppered" through the script crate, exposing SpiderMonkey
   concepts such as untyped `JSObject` directly.
2. **Garbage-collection integration** — Servo manages *Rust object lifetimes with
   SpiderMonkey's collector* (the Josephine design).
3. **WebIDL codegen** emitting SpiderMonkey-specific glue.
4. The `mozjs` / `mozjs-sys` bindings plus the utilities in
   `components/script/dom/bindings`.

**Category 2 is the crux, and it is also exactly where our benefit lies.** This is not
"swap a VM behind an interface": Servo's DOM objects are owned and traced by SpiderMonkey's
collector, so changing engines means **re-homing the DOM's memory-management model**. That
is the largest and riskiest part of the work — and it is precisely the part that would
deliver the memory-safety property §11.8 is after, because it is the DOM lifetimes that
currently depend on an unsafe collector.

**Servo's own incremental path is the right on-ramp.** Upstream is progressively hiding
SpiderMonkey APIs behind safe idiomatic Rust (the WebGPU refactor replaced unsafe
`JSObject` returns with typed concepts). That work is useful regardless of which engine
ever lands, is upstreamable, and produces the enumeration of SpiderMonkey interactions that
any generic interface must be designed against. It is possible to contribute there and gain
both leverage and knowledge without committing to the whole project.

**The engine-side unknown, which must be settled before anything else:** can the candidate
host *foreign* objects — the DOM — and trace into Rust data? Boa's `boa_gc` has
`Trace`/`Finalize` derives for user types, which is promising but unproven at DOM scale.
Nova's arena-and-handle design is a different shape and may make foreign hosting easier or
harder; unknown. **This is a spike, not a port**, and it gates everything downstream.

**A live data point for why this matters:** Servo 0.3 shipped SpiderMonkey updates *to fix
memory-safety bugs*. The concern is a recurring maintenance reality, not a theoretical one.

**Honest scale:** "less work than writing our own engine" remains true, and should not be
read as "small". Re-homing the DOM lifetime model of a browser engine is a major project.
The argument for it is that it is *bounded, upstreamable, and reuses two mature codebases*,
where writing our own is none of those.

**Recommended next steps, cheap before expensive:**

1. **Unsafe-surface audit** of `boa_gc` and Nova's collector against §11.8's criterion —
   how much `unsafe`, how concentrated, how auditable. Days.
2. **Foreign-object hosting spike** — can Boa hold and trace a non-trivial Rust object
   graph shaped like a DOM subtree? Days to weeks, and it decides whether the candidate is
   viable at all.
3. **Contribute to Servo's de-SpiderMonkey cleanup** — useful regardless, upstreamable,
   and the fastest way to learn the true scale of categories 1 and 3.
4. Only then estimate category 2 with evidence rather than a guess.

### 11.9.3 Placement does not rescue this, because we do not control the servers

atrium-navigator.md §3.2's JS-free-client invariant puts the legacy engine *server-side*,
which appears to move this gap off the user's machine. It does not, in general, because
that clause means **vendor-hosted** (insula.md §0.6.4) — and the sites that most need tier
4 are precisely the ones that will never cooperate. One cannot ask a bank to host Servo.

The honest placement matrix:

| who runs the engine jail | works for | privacy cost | where the gap sits |
|---|---|---|---|
| **the site's vendor** | only cooperating sites | none | vendor's server |
| **an operator-run rendering proxy** (ours or a provider's) | any site | **sees all browsing** | operator's server |
| **the user's own always-on machine** (home server, desktop serving a phone) | any site | none — self-hosted | user's own infrastructure |
| **localhost jail** (default fallback) | any site | none | **the user's device** |

**Policy: never default to an operator-run rendering proxy.** It works for every site and
is genuinely useful for thin clients, but it reintroduces exactly the third party this
architecture exists to remove — an operator who sees every page a user visits. It may be
offered; it must be an explicit, informed opt-in, never a default and never silent.

So the working answer for arbitrary existing sites is a **localhost jail by default**,
with self-hosted placement on a machine the user controls as the better option where one
exists — which is a real option rather than a hypothetical, since location transparency
makes it the same binary either way.

Which means §11.9's gap does sit on the user's device in the default configuration. What
bounds it is the jail, JIT-off (§11.5) and session recycling (§11.9.1) — not placement.

### 11.9.4 The adoption risk, stated

The native model is a **proposal to an ecosystem that may not adopt it**. That risk is
taken deliberately, and it is worth being explicit about how it is bounded:

- **Open protocols need nobody's permission.** Mail, calendar, contacts and chat can be
  served by native jailed apps today, against IMAP/JMAP/CalDAV/XMPP/Matrix. The
  highest-stakes category (§11.8.2) is reachable with zero site cooperation, which makes
  it the natural beachhead rather than a later phase.
- **Servers under our control can serve the native way immediately**, demonstrating the
  model end-to-end without waiting for anyone.
- **The adoption risk is therefore concentrated in proprietary SaaS** — which is exactly
  the residue that needs tier 4. The risk is real, but it is a known and enumerable set
  rather than "the whole web".
- **Tiers 1–3 must be good enough that adoption is not required for the system to be
  worth using.** If reading the web works well without any site changing anything, the
  proposal gets time to be adopted or not.

### 11.9.5 Why this is tolerable in the meantime

Not because the risk is small, but because of where it sits:

- **Tier 4 is the explicitly-marked legacy lane, and the lane is visible** (§8). Its weaker
  posture is disclosed and chosen, not hidden behind a uniform claim of safety — and
  §11.9.3 means that disclosure has to include *where the engine is running*.
- **The thesis already answers "I want a secure interactive app": make it a native jailed
  app.** Tier 4 is a bridge for content that predates that answer, not the future the
  architecture is arguing for.
- **Whether it matters at all is a measurement, not a guess.** §5.4's corpus number decides
  both whether tier 4 gets built and whether its architecture is a rounding error or the
  system's principal exposure. If the legacy lane turns out to carry real traffic, §11.8's
  trigger 2 fires and this stops being tolerable — by design.

## 12. Open questions

1. **Zygote fork cost under Portcullis** — measured, not assumed, and it decides §7.6's
   origin-vs-site granularity.
2. **Carrying the engine's accessibility tree into our semantic tree** — feasibility is a
   port question and determines whether the legacy lane keeps G5.
3. **Freshness for prerendered artifacts** — a Nomenclator question (backend spec §10.3).
4. **Whether the declarative vocabulary of §5 is v1 or v2** of the document profile; it is
   not needed for reading, but it is needed before anyone authors anything interactive.
5. **Remote placement policy** — when, if ever, a remote engine jail is offered by default,
   given it reintroduces a third party who sees browsing.
6. **Where the data-access boundary sits for hydrating pages** — §5.4.1 shows the reading
   lane's remaining loss is cross-origin data a page needs to rebuild what it tore down,
   not engine fidelity. Relaxing it for same-site data would recover those documents and
   reintroduce a privacy cost; refusing it makes tier 1 the answer for that population.
   This is now the decision that matters, and it is a policy one.
