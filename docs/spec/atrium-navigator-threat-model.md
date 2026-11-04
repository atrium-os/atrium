# Atrium Navigator — threat model: browsing a malicious site

*Brainstorm, 2026-11-04. Status: proposed. It names gaps and ranks fixes; none of the fixes
below is settled until the user decides it.*

The question: a user opens a URL whose owner is hostile. What can the owner make happen,
how far does it reach, and what still has to be built so the answer is "not much"?

The method: follow the bytes. Each stage a hostile page touches is listed with what the
specs promise, what the code enforces **today** (checked against the tree on 2026-11-04,
not inferred from the specs), what an attacker gets if that stage falls, and the fix.
Companion specs: [backend](atrium-navigator-backend.md) (trust topology §2, validator §4.2,
worker §4.7) and [legacy web](atrium-navigator-legacy-web.md) (network §5.4.1b–d,
exploration §5.4.3, integrity §5.6, isolation §7, islands §8.1, monitor §10, tier-4 §11.9).

## 1. Who the attacker is, and what they want

| attacker | wants | typical vehicle |
|---|---|---|
| **A1 — exploit author** | code execution on the user's machine, then persistence or data | malformed bytes aimed at a parser: JS, HTML, CSS, fonts, images, the scene graph |
| **A2 — phisher** | the user's credentials or money, with no bug at all | a faithful-looking page, a lookalike origin, a fake dialog |
| **A3 — tracker** | identity and browsing history across sites | subresource fetches, fingerprinting, request templates |
| **A4 — lateral mover** | things only the user's machine can reach: LAN devices, localhost services | URLs and DNS answers that point inward |
| **A5 — poisoner** | to change what *other* readers of a page see | a bad artifact in a shared store |
| **A6 — nuisance** | a hung or exhausted machine | CPU, memory, disk and GPU bombs |

The architecture is strong against A1 and A3 *by construction* in the document lane, and
weak in places against A4, A5 and A6 *by omission*. A2 is a product problem that no
isolation design solves alone.

## 2. Stage by stage

Risk is rated **today** (with the code as it is) and **as designed** (once the spec's
own promises are built). "High" means a realistic attack with a large blast radius.

### 2.1 Fetch — `navigator-fetch`, `navigator-fetchd`

**Enforced today.** Capsicum capability mode with casper `cap_net`: after `cap_enter()` the
fetcher can name no file and can connect only to an address casper resolved, on ports 80
and 443. One bounded GET, at most 5 redirects, an https→http redirect is refused, the body
is capped at 8 MiB, and no Cookie, Referer or Authorization header is sent (§5.4.1b). The
page network is same-origin GET (§5.4.1c).

**Gap — the LAN and localhost are reachable (A4).** Nothing in the fetcher refuses a
non-global address. Three routes reach inward:

1. The user follows a link to `http://192.168.1.1/` or `http://localhost:8080/`.
2. **DNS rebinding.** `evil.example` resolves to a public address for the first fetch and to
   `127.0.0.1` or `10.0.0.5` for the next one. The second request is "same origin" by name,
   so §5.4.1c admits it.
3. A redirect from a public URL to a private one.

What the attacker gets: a GET against a router admin page, a printer, a dev server, or a
cloud metadata endpoint (`169.254.169.254`), and the *response converted into a document*.
If that conversion lands in a shared store (§2.6), the leak extends past this user.

**Fix (P1).** After resolution and before connect, refuse loopback, RFC 1918, link-local,
CGNAT, ULA, multicast and metadata ranges. Pin the resolved address for the whole
conversion, so a rebinding answer cannot change it partway through. Casper's `NAME2ADDR`
then a connect to the checked address keeps the "only what casper resolved" property.
Intranet browsing becomes an explicit, per-network user grant whose conversions never
leave the user's dedup domain.

**Rating:** today **high** for A4. Once fixed, low.

### 2.2 Conversion — `navigator-prerender` (Boa, the state recorder)

This is the stage where **attacker-written code runs**. Boa executes the page's scripts;
exploration (§5.4.3) clicks the page's own handlers.

**Enforced today.**
- Conversions carry no credentials (§5.4.3 invariant 1).
- The page network is closed during exploration, and navigation is refused.
- There is a wall-clock deadline (`PRERENDER_DEADLINE`).
- One child process per conversion.
- The network is held by a jailed `navigator-fetchd` when `PRERENDER_FETCHD` is set.
- Boa is memory-safe Rust above its collector, with JIT off (§11.5).

**Gap — the converter itself is not confined (A1).** The jail wraps the *fetcher*, but the
process running hostile JavaScript is an ordinary child with the user's full authority. I
found no confinement for it in `navigator-prerender/src/main.rs`. A Boa bug is not
hypothetical: the engine has `unsafe` code in its GC and object model, and this project has
already fixed a Boa panic on a corpus page (c8100f2). A memory-corruption bug there is code
execution as the user. Today that is the **single largest exposure in the system**,
larger than tier 4, because tier 4 is not built and this lane runs on every page.

**Fix (P0).** Run each conversion through the same `portcullis exec` launcher the document
worker already uses (backend §4.7a, measured on 98 jails). Give it zero capabilities: it
reads its input on stdin, writes its recording to stdout, and talks to fetchd over an
inherited descriptor. The mechanism exists and has been measured; this only connects it.

**Gap — no memory or CPU ceiling (A6).** A page can allocate until the machine swaps, or
spin until the deadline on every conversion. **Fix (P2):** `rctl` limits per conversion
jail (memory, CPU), and a total fetched-bytes budget per conversion as well as per response.
8 MiB per response times hundreds of subresources is otherwise a disk-fill.

**Exploration cannot act.** The network is closed and navigation is refused, so a
simulated click cannot reach `/delete?id=`. Keep this invariant when §5.4.5's
perturbed-replay design lands: replays must use the prerender's *recorded* responses,
never the live network.

**Rating:** today **high** (A1). With the jail, medium: the converter can then only lie
in its output, which §2.3 and §2.6 handle.

### 2.3 Normalize and render — the document worker

**Enforced today.**
- One worker process per document, with a pipe as its only channel.
- The pipe is read as untrusted, with bounded frames, deadlines, and retirement on a lie.
- The Document Profile ceilings are refused at ingest.
- The validator is fuzzed with reached-coverage asserted (§4.7b).
- Under `Confinement::Launcher` the worker runs in a signed, zero-capability jail (measured
  in §4.7a).

**Gap — confinement is opt-in.** `JailedHost` jails only when built with a launcher.
`unconfined_for_testing` is honestly named and `require_confinement()` exists, but nothing
yet *calls* `require_confinement()` on the production path. **Fix (P1):** the shipped
`navigatord` refuses to start without a launcher; unconfined mode is a test-build feature.

**What a compromised worker gets.** Nothing on the machine. Its only output is a scene
graph, which the broker validates for size and shape. §10.5 admits that the *content* of a
valid scene graph is a steganographic channel. That channel leads only to the compositor
and the screen, so it matters only when combined with a compositor bug (§2.5).

**Rating:** today medium (an unjailed default), designed **low**.

### 2.4 Images and fonts

**Fonts.** Web fonts go through `navigator-fonts`, which canonicalises and sanitises them
into one sfnt: an admitted-table allowlist, unhinted, content-addressed. Hinting bytecode,
the historic font RCE vector, is gone. The parsers (`ttf-parser`, `swash`) are safe Rust.
Rating **low**. Keep the sanitiser inside a jail (it runs in the converter, so P0 covers
it).

**Images — gap: production decoding has no home yet (A1).** The converter reads only image
*headers*, for intrinsic size. The renderer never sees image bytes; it refers to them by
content address. The only decoder in the tree is the `png` crate in the `raster` test tool.
So **where a hostile JPEG/PNG/WebP is decoded for display is not specified**. The easy wrong
answer is "Fresco decodes it". That puts a decoder shared across every site inside the
compositor, which is exactly what §7.3 forbids. Decoders are historically the richest
source of browser RCE.

**Fix (P1), to settle before images reach the screen.**
1. Decode inside the per-document worker jail, or in an ephemeral per-image jail.
2. Use memory-safe decoders (`png`, `zune-jpeg`, `image-webp`).
3. Hand Fresco only raw pixel buffers whose dimensions the validator has already capped.
4. Refuse an image whose header dimensions exceed the profile *before* allocating the frame
   (the decompression bomb).
5. Never decode in Fresco.

**Rating:** undefined today, low once fixed.

### 2.5 Composition and GPU — Fresco, Limen, the RDNA4 driver

Every site's output converges here, so this is the **shared, privileged choke point**.
§7.2 calls the compositor safe to share because its job is to serve mutually distrusting
clients. That holds only while its input language stays small.

What Fresco consumes:
- validated scene graphs;
- glyphs from sanitised fonts (Fresco also parses fonts itself, in
  `fresco-scene-server/src/render/font.rs`);
- with §2.4, pixel buffers.

What sits behind it:
- the custom GPU kernel driver;
- with `FRESCOD_RENDERER=gpu`, the RDNA4 kernels.

A Fresco compromise is cross-site, because it sees every window. It is also one ioctl away
from kernel code.

**Mitigations, mostly existing.**
- Apps never get direct GPU fds in production (GPU ABI decision).
- Crash and stall recovery exists (GPU ABI §10.1–10.3), so a scene that hangs the GPU costs
  a reset, not the machine.

**To add (P2).**
- Fuzz Fresco's own scene ingest, not only the broker's validator.
- Assert scene limits inside Fresco as well, as defence in depth.
- Have Fresco take glyph *outlines* or pre-shaped runs from the jailed side rather than
  parsing fonts.
- Fuzz the GPU driver's ioctl surface with the same reached-coverage discipline as §4.7b.

**Rating:** medium. Few bugs are likely, but the blast radius is the whole system.

### 2.6 The shared store — conversions in Tessera CAS (A5)

A conversion is keyed by its source bytes and served to every later reader of those bytes
(§5.6). A converter that lies, which is a compromised one or, after §2.2's fix, a jailed
one with a bug, can write an artifact that differs from what an honest converter would
produce. That artifact then reaches everyone who shares the store.

**Fix (P1).**
- The default dedup domain for conversions is **per user**.
- Sharing across users or machines admits an artifact only when it was *reproduced*: a
  second, independent conversion of the same recorded source bytes and environment yields
  the same artifact hash. §5.6 already asks for reproducibility; this makes it the
  admission rule rather than an audit tool.
- Conversions of private-network or authenticated pages are never shareable (§2.1, §7.3).

**Rating:** low on a single-user machine, **high** the day stores are shared. So this
must be decided before sharing ships, not after.

### 2.7 Data bindings and request templates (§5.4.5, proposed)

Once §5.4.5 lands, a document carries *declared* requests that the backend replays at view
time to refresh a binding. A hostile page can declare whatever it likes.

**Risks.**
- A3: a beacon that phones home on every view.
- Probing other origins.
- When logins exist, a state-changing same-origin GET.

**Rules the broker enforces at replay (P2, design-time).**
- Same origin as the document, GET only.
- The URL template comes from the recording; only typed slots are filled, never a
  free-form URL.
- Replays happen only on user action or on the declared refresh interval, with a floor.
- The request goes through the per-origin fetcher of §7.7.
- It is reported to the monitor (§10.5) if it ever names another origin.

### 2.8 Credentials and logins (§7.7)

There are none today, which is why A1 currently yields no account takeover. The day a
cookie jar exists:
- it lives in a per-origin fetcher, never in the converter (§5.4.3 invariant 1);
- an authenticated conversion is never shareable;
- password autofill is bound to the exact origin, so a lookalike gets nothing.

This is a **gate**, not a feature: no login support ships before per-origin partitioning.

### 2.9 Tier 4 and islands — Servo, SpiderMonkey, the CDM (§8.1, §11.9)

This is the weakest lane by the spec's own admission (§11.9). An island engine compromise
yields that origin's session data and the island's allowed channels: the fetcher, the scene
pipe, and the typed island channel.

**Additions this review surfaces.**
- **Overlay on an island (A2, clickjacking).** In the island shape the document lane draws
  around and possibly *over* a jailed island. The cases that matter are a consent prompt,
  a purchase button and a "subscribe" click. A hostile document can lay transparent or
  decoy content over them.
  - **Fix:** Limen composites islands above document content.
  - Input reaches an island only while it is fully unobscured for a minimum dwell time,
    which is IntersectionObserver v2's rule, enforceable here because Limen does the
    composing.
- **The CDM's Linux ABI emulation is extra kernel surface.** The linuxulator has a long
  CVE history, and the CDM is an unauditable binary.
  - The CDM jail already has no network and no filesystem beyond its own state.
  - **Prefer a bhyve micro-VM for the CDM island** when the hardware allows. A guest kernel
    boundary is far stronger than a syscall translation layer.
  - Where the hardware doesn't allow it, run the CDM inside a Capsicum-restricted process
    under the jail, not as a bare process in the jail.
- **Typed island channel.** Treat it like the worker pipe: bounded frames, a fixed message
  set, fuzzed, and the document side never trusts a value from the island.

### 2.10 Navigation, links and downloads

- **Schemes.** Links may name only `https`, `http` (marked), and in-document anchors.
  Refuse `file:`, `javascript:`, `data:` navigations and OS scheme handlers unless the
  user allowlists them. There is no `javascript:` vehicle in the document lane, but the
  scheme still has to be refused at the link-activation layer, not left to happen not to
  work.
- **Downloads.** Saved with the source origin recorded, never executable by default, and
  opened only by a jailed viewer.

### 2.11 Deception — phishing without a bug (A2)

The document lane has an uncomfortable property: it renders every page in the same clean,
consistent typography. That makes a phishing page *more* credible, not less.

- **Trusted origin display** sits in Limen chrome that no scene graph can draw into or
  cover. That covers fullscreen too: fullscreen always shows a non-dismissable origin
  strip.
- **The registrable domain is emphasised**, and IDN shown as punycode when the script is
  mixed or confusable (UTS #39 skeletons).
- **No page-drawn dialogs** that look like system ones. A tier-4 `alert()` renders inside
  the island's frame, labelled with its origin.
- **Forms.** A password field on a non-https page, or on an origin never seen before, gets
  a visible warning. Autofill never fills a lookalike (§2.8).

### 2.12 What the architecture already removes

These belong in the model because they are *wins*, not because they need work:

- **The JS-free document lane has no scripted timers and no scripted fetch.** Spectre-class
  probes, cache-timing oracles and most fingerprinting need script running at view time.
  None runs (§7.4, profile §1.1).
- **Viewing fires no tracker.** Subresources are fetched once at conversion and served from
  the store, so third-party pixels are not re-fetched per view. They are still fetched
  once, from the user's IP, at conversion (§5.4.1d), and the typed-subresource rule is what
  bounds that.
- **The worker's memory never holds two documents**, so cross-document memory disclosure
  is structurally absent (backend §2).
- **The broker never parses**, and authority never coexists with hostile input.
- **Honest identification (§5.4.1b) means cloaking is uninteresting.** A site that serves
  Navigator users different content is serving them that content. The cloaking that
  matters is §2.6's poisoning, not UA sniffing.

## 3. Ranked work list

| # | fix | stops | cost | where |
|---|---|---|---|---|
| **P0** | Jail the converter process (Boa) with zero capabilities via `portcullis exec` | A1 → code execution as the user | small: the launcher exists and was measured (§4.7a) | §2.2 |
| P1 | Refuse non-global addresses after resolution; pin the address per conversion; intranet as an explicit grant | A4: LAN and localhost, DNS rebinding | small | §2.1 |
| P1 | `navigatord` refuses to start unconfined outside test builds | A1 against the worker | trivial | §2.3 |
| P1 | Decide where images decode: per-document jail, memory-safe decoders, never in Fresco | A1 via decoders | design and medium code | §2.4 |
| P1 | Per-user dedup domain by default; reproduce-to-share | A5 poisoning | design now, code with sharing | §2.6 |
| P2 | `rctl` memory/CPU limits on converter and worker jails; per-conversion byte budget | A6 | small | §2.2 |
| P2 | Fuzz Fresco scene ingest and the GPU ioctl surface; Fresco takes outlines, not fonts | A1 against the shared compositor | medium | §2.5 |
| P2 | Broker rules for replayed request templates | A3, CSRF | design-time, with §5.4.5 | §2.7 |
| P2 | Unobscured-input rule for islands; bhyve for the CDM | A2 clickjacking, kernel surface | medium, with §8.1 | §2.9 |
| P2 | Scheme allowlist on link activation; download marking | A1/A2 | small | §2.10 |
| P3 | Origin chrome, punycode for confusables, form warnings | A2 | product work | §2.11 |
| gate | No login support before per-origin credential partitioning (§7.7) | account takeover | — | §2.8 |

## 4. The one-paragraph answer

Against a malicious site **today**, the realistic serious outcome is not escaping a jail.
It is **attacker-written JavaScript running in an unjailed converter**, plus a **fetcher
that will fetch the user's LAN**. Both are small fixes whose mechanisms already exist. With
those done, the document lane's position is unusually strong:
- hostile code runs only at conversion, in a zero-capability jail, with no credentials and
  no network of its own;
- what reaches the screen is a validated, bounded scene graph;
- nothing runs at view time.

What remains is the shared compositor and kernel driver, the shared-store policy, and
tier 4. The last is weak by the spec's own admission, and §8.1's islands shrink it to the
one element that needs it.

## 5. Open questions, with recommendations

*Recommendations, 2026-11-04; the user has not decided them yet.*

1. **Is the converter jail's cost acceptable?** *Recommend: jail unconditionally, and
   measure the cost to report it, not to decide whether to jail.* A conversion already takes
   seconds of Boa time. §4.7a's worker jails added about 3 s over 98 documents, which is a
   rounding error next to that. If the cost ever matters, the zygote (§7.5) is the fix; an
   unjailed path is not.
2. **The intranet grant: per network or per host?** *Recommend: per origin, and bound to
   the network it was granted on.* The gateway's identity, not the SSID, which anyone can
   copy. A per-network grant opens every device on the LAN to every page that rebinds to
   it, which is the attack itself. A per-origin grant (`http://192.168.1.1`, on this
   network) is least privilege and costs one prompt per device. Conversions under a grant
   are never shareable.
3. **Can conversions be bit-reproducible across machines?** *Recommend: don't assume it;
   build for it and measure it.*
   - Pin every input the converter reads: clock, `Math.random` seed, timer order, ICU
     data, font set, the converter's own build hash.
   - Then convert each corpus page twice, on two machines, and compare hashes.
   - Reproduce-to-share admits only what matches. A page that doesn't reproduce stays in
     the user's own domain, which is the safe failure.
   - The match rate is the number that says whether sharing is worth building at all.
4. **Should Fresco ever parse a font?** *Recommend: only the shipped, signed font set,
   never a web font.* Fresco needs fonts for its own UI, and those are trusted data (§7.2).
   Web fonts reach it as glyph outlines, produced in the jail from the sanitised sfnt. The
   validator bounds them like any other path: points per glyph, glyphs per run. That
   removes the one place Fresco parses a hostile binary format.
