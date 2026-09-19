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

## 11. Open questions

1. **Zygote fork cost under Portcullis** — measured, not assumed, and it decides §7.6's
   origin-vs-site granularity.
2. **Carrying the engine's accessibility tree into our semantic tree** — feasibility is a
   port question and determines whether the legacy lane keeps G5.
3. **Freshness for prerendered artifacts** — a Nomenclator question (backend spec §10.3).
4. **Whether the declarative vocabulary of §5 is v1 or v2** of the document profile; it is
   not needed for reading, but it is needed before anyone authors anything interactive.
5. **Remote placement policy** — when, if ever, a remote engine jail is offered by default,
   given it reintroduces a third party who sees browsing.
