# Atrium Navigator — the browser, reimagined without JavaScript

Status: design (settled direction 2026-06-17). The D6 "web" story. Consolidates the
browser-reimagined view that follows from insula.md §0.5–0.6 (category collapse),
and ties together `atrium-doc`, Nomenclator, Opifex, Limen, and Aqueduct remote
sessions. This is a *navigator + document viewer + app launcher*, not a JS runtime.

> **One sentence.** A browser is a JavaScript runtime *by accident*; Atrium's jail is
> the sandbox the web only ever simulated in-process, so the navigator runs **native
> jailed apps** (local or remote) and renders **documents as inert content** — and the
> *client never has to run JavaScript at all*.

## 1. Why the browser is a JS runtime (and why Atrium needn't be)

The entire JS / V8 / same-origin edifice exists for one reason: a browser must run
*untrusted code* safely, and the OS historically gave it no way to sandbox a process.
So the browser built an **in-process** sandbox (V8) and forced everyone into the one
language that sandbox runs (JavaScript). Everything else — the DOM-as-app-runtime, the
framework churn, the per-tab process gymnastics — is downstream of that single
constraint.

Atrium removes the constraint. It sandboxes at the **OS level**: a Portcullis jail +
a dedicated non-root per-app uid + a capability manifest + a signed bundle (see
`portcullis.md` §9, `atrium-bundle-format.md`). Untrusted code is contained by the
*kernel*, not by a language VM. Therefore:

- the in-process sandbox is **redundant**, so
- the JS-only constraint **evaporates** — an app is native code in any language
  (Rust/C/…/WASM-AOT); JS becomes *an* option an app may embed, never the mandate;
- "click a link, run untrusted code" is **safe with native code**, because the jail
  contains it — and the capability model is *stronger* than web origins (which have a
  long history of V8 RCEs escaping the in-process sandbox).

## 2. The decomposition

The monolithic browser dissolves into four pieces Atrium already names:

| browser job | Atrium piece | runs code? |
|-------------|-------------|------------|
| **render content** (HTML/CSS/MD/PDF) | **`atrium-doc`** — content viewer, doubly-jailed, **zero capabilities, zero JS** | no |
| **run "web apps"** | **Insula apps** — native, jailed, capability-gated | yes (in a jail) |
| **resolve URLs** | **Nomenclator** — `atrium-app://` / `atrium-doc://` names → content hashes | — |
| **compose surfaces** (the `<iframe>`) | **Limen** — cross-jail Fresco composition | — |

The **navigator** is the thin shell that follows a name, decides *document vs app*,
and either renders (atrium-doc) or launches/connects (the app). It holds no special
authority itself; it is a launcher and a viewport, not a runtime.

### 2.1 Documents — the casual-browse 99%, fully JS-free
"Follow a link, see content, follow another link" survives *completely* for content:
Wikipedia, news, blogs, papers, recipes, search results, READMEs. These are **bytes,
not programs** — `atrium-doc` renders them with **zero ambient authority and no script
execution**. Content addressing (`atrium-doc://<hash>`) gives integrity + dedup +
offline. This is the bulk of what people actually "browse," and it needs no JS engine.

### 2.2 Apps — "web apps" become native jailed apps
What was a web app becomes an Insula app, delivered with web-like UX:
`atrium-app://name` → Nomenclator → **Opifex** fetches the signed bundle → Portcullis
allocates a per-app uid + jail + launches. Zero-install is preserved; a **trial-launch**
pattern (insula.md §0.5.3) gives frictionless onboarding *without* the web's
"accidentally accumulate trust" failure mode — apps get an explicit (light) consent
moment because they receive capabilities; documents never do.

## 3. Execution model — native binaries, jailed, location-transparent

**The primary app is a native Atrium binary in a jail.** Compiled native code
(Rust/Pergola/C against libatrium/fresco-client), contained by a Portcullis jail.
The jail is the invariant; *where* the jail runs is a deployment knob, not an app
rewrite — because the jail boundary and the Fresco protocol are identical on either
side. This is **location transparency**: the *same* binary, unmodified, runs either
place (§3.1). (Contrast the web, which forces SSR-vs-SPA app-architecture choices
and ships code to every client.)

**WASM-IR → AOT → native is a *secondary* on-ramp**, not the center: a portability /
language-reach lane (cross-arch distribution per insula.md §3.3, plus non-native
languages — JS among them, §3.2) whose **output is still a primary-model artifact** —
a native jailed binary. It converges on the primary model; it doesn't replace it.

### 3.1 The two placements of one jailed binary

A navigated app runs in one of two places, both already supported by Atrium pieces —
*the same native jailed binary*, just placed differently:

- **Client-side native (default).** An ephemeral *local* jailed native app. Best
  latency, offline, privacy, performance. "Web app" → local jailed binary.
- **Server-side native (the legacy-web bridge + thin client).** A *remote* jailed app
  whose **Fresco scene graph streams to the client** over Aqueduct (the remote-session
  / VDI property; see `project_aqueduct_remote`). The app's code runs on a server (the
  vendor's, or a jailed sidecar); the client receives only a native scene graph to
  render.

> **The JS-free-client invariant.** The client device **never runs JavaScript**. New
> apps are JS-free end-to-end. The *legacy* web — services that exist only as
> HTML+JS — is reached by running that web engine **server-side** (insula.md §0.6.4,
> "vendor-hosted web access") and streaming the resulting Fresco scene graph. The JS is
> *relocated to the server*, not run on your machine. The client stays a pure native
> Fresco renderer + jail, for everything.

### 3.2 JS and dynamic languages as Atrium apps (the secondary lane)

When we *do* want JavaScript (or any non-native language), the answer is **not** to
embed V8 and not to port a browser engine. Two goals must stay separate:

- **Run the *existing* web** → impossible to escape the web platform (DOM, JS-as-
  specced, fetch/CSSOM); every site + framework targets it. So legacy web = **Servo,
  server-side** (§6), per the JS-free-client invariant. AOT/no-DOM does *not* serve
  this goal — don't try.
- **Let JS/TS developers write *Atrium* apps** → the secondary on-ramp (§3): the
  language compiles to **WASM-IR → AOT (Cranelift) → native jailed binary** (insula.md
  §3.3), binding to the **scene graph / Pergola, not a DOM**. The result is a
  primary-model native jailed app whose source happened to be JS.

Two properties make this only-possible-on-Atrium, and they're the point:

1. **Strip the in-engine sandbox.** V8's heap sandbox, isolates, JIT hardening,
   Spectre mitigations, and RWX JIT pages all exist to run untrusted code
   *in-process*. The jail makes them redundant. An Atrium JS runtime is a fraction of
   the size, and **AOT (no JIT) → W^X holds → Capsicum-clean → tiny TCB**.
2. **No DOM.** The DOM is the web's retained-tree+CSS artifact; Atrium has a better
   native one (Fresco scene graph + Pergola). New apps bind to it directly.

Honest caveat: JS is the *hard* AOT case (dynamically typed, prototypes, `eval`) —
unlike the typed/structured SPIR-V and WASM that Tier-2/Cranelift AOT cleanly. What
ships is a *baseline* AOT (boxed, dynamic-dispatch) — fine for UI code, not JIT-grade
peak speed — traded for the simplicity + security wins above. Fully-dynamic JS rides
QuickJS-on-WASM; TypeScript-subset / AssemblyScript / cleanly-compiled languages AOT
well. The real primitive is the WASM-IR→AOT road (§3.3 of insula.md); JS is just one
(awkward) source language on it — never a from-scratch engine.

## 4. Delivery, naming, distribution

- **Naming:** `atrium-app://<name|hash>` (programs), `atrium-doc://<hash>` (content);
  Nomenclator maps human names → content hashes through publisher manifests. A user
  picks any registry they trust (no single app store).
- **Install/launch:** Opifex (binary) / insula (source-ports) produce + place the
  signed `.insula` bundle; Portcullis jails + launches. Trust roots in the publisher
  signature + the capability prompt.
- **Portability:** WASM as a *distribution IR* (insula.md §3.3), AOT-compiled to native
  at install — cross-*arch* reach without JS, never a runtime sandbox.

## 5. Honest limits (what is NOT free)

1. **The existing web's *apps* don't become native by magic.** Gmail/Figma are either
   rewritten as Insula apps or run server-side and streamed. This founds a *new* native
   ecosystem; it does not migrate the old one.
2. **Reach is Atrium-only.** The web runs on any device, zero-install, anywhere; a
   native jailed app runs where the Atrium runtime exists. WASM-IR buys cross-arch, not
   cross-OS, reach.
3. **Server-side streaming costs latency + a server + a network** (not offline). It is a
   complement to local-native, not a universal replacement.
4. **Frictionless holds only for documents.** Content needs zero trust; apps get a light
   consent gesture (correct, but a real difference from drive-by web).
5. **The §0.6.4 edge case is real.** Much of the world is web-only; until services are
   native or hosted/streamed, the server-side bridge (and an eventual server-side HTML+JS
   engine) is required.

The wins in exchange: a *stronger* sandbox than the web, escape from JS-as-the-forced
client runtime, a clean program/content split, and a coherent native app model.

## 6. Where the rendering engine fits (Servo, last and server-side)

A full HTML+CSS+JS engine (Servo/WebRender, the old D6 item) is the **heaviest** piece
and is needed only for the legacy-web-content tail. It should slot in **last**, and
ideally **server-side**, so the client stays JS-free per §3. `atrium-doc`'s content
path (static HTML/CSS/MD/PDF, no script) covers the casual-browse majority without it.

## 7. Build order (D6)

1. **Navigator + `atrium-doc` (content path).** Resolve a name → render content
   (HTML/CSS/MD/PDF → Fresco scene graph), no JS. Buildable on today's stack; covers
   most "browsing."
2. **App-launch flow.** `atrium-app://` → Nomenclator → Opifex → ephemeral jail +
   trial-launch. "Click a link, run a native app."
3. **Limen composition.** Embed multiple jailed surfaces (the `<iframe>` replacement).
4. **Server-side Fresco streaming.** The legacy-web + thin-client bridge, on the
   Aqueduct remote-session transport — the JS-free-client invariant for legacy apps.
5. **HTML+JS engine (Servo), server-side.** The legacy-content tail, last.

## 8. Relationship to other specs

- `insula.md` §0.5–0.6 — the category-collapse argument this consolidates; §0.6.4 the
  vendor-hosted-web edge case; §10.6 `atrium-doc`.
- `atrium-bundle-format.md` — the signed bundle apps are delivered as.
- `portcullis.md` §9 — the jail/uid/capability sandbox that replaces V8.
- `toolkit-backends.md` — how native apps (incl. ported toolkits) render; the
  self-composite path the navigator's apps use.
- `project_aqueduct_remote` (memory) — the remote-session transport for server-side.
