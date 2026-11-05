# Atrium Navigator — threat model: browsing a malicious site

*Brainstorm, 2026-11-04. The user accepted all recommendations the same day (§3, §5), settled
how Fresco is jailed (§2.5.1), and asked for the credential design (§6).*

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

**Built (2026-11-05).** `navigator_fetch::addr` + `Conversation`:
- **The check is on the resolver's answer.** The `Connect` seam is split into `resolve`
  and `connect_addr`. Every answer is checked before any connect. Under capability mode
  that is casper `NAME2ADDR`, then the check, then `cap_connect` to that address, so
  `CONNECTDNS` still guarantees the process connects only where casper resolved.
- **Refused:** loopback, `0/8` and `::`, RFC 1918, link-local (`169.254/16`,
  `fe80::/10`), CGNAT `100.64/10`, ULA `fc00::/7`, multicast, broadcast, the
  documentation, benchmarking and reserved ranges, `169.254.169.254` and
  `fd00:ec2::254`.
- **IPv6 is an allowlist.** Only `2000::/3` minus its carve-outs is admitted. The
  forms that embed IPv4 (mapped, NAT64 `64:ff9b::/96`, 6to4) are judged by the IPv4
  address they reach.
- **Literal-IP URLs and every redirect hop take the same path.** The URL parser has
  already normalised `http://2130706433/` and `http://0x7f.1/` to `127.0.0.1`.
- **A mixed answer (one public, one private) refuses the whole host.**
- **Pinning.** One `navigator-fetchd` process is one conversion. Its `Conversation`
  pins the first answer per host. The same host and port is never resolved again. The
  other port must answer with an already-pinned address, and only those are used. A
  rebinding answer is refused by its range, and under the grant by the pin
  (`FetchError::Rebinding`).
- **The grant.** `NAVIGATOR_FETCH_GRANT_PRIVATE_NETWORK=1` or `navigator-fetchd
  --grant-private-network`, off by default. It admits loopback, RFC 1918, link-local,
  CGNAT and ULA, and never the metadata endpoints. It is process-wide, a stand-in for
  §5 q2's per-origin grant, and a jailed fetchd (launched with no arguments) cannot
  receive it. That is the safe direction.
- **Tests.** 11 tests cover every range at both edges, both families, the embedded
  forms, a redirect to a private literal and to a privately resolving name, rebinding on
  the same and the other port, and the grant. They were positive-controlled: an `admit`
  that accepts everything fails 9 tests, and removing either half of the pin fails its
  test. `navigator-fetchd --gate` gains seven refusal rows under capability mode,
  including `http://localhost/` resolved *by casper*, plus a grant control arm.
- **The prerender's curl `HttpFetcher` does not apply the rule, by design.** It is the
  unconfined measurement instrument. A confined conversion refuses to run without
  fetchd (§2.2), so curl is never on a real conversion's path. See the comment on
  `HttpFetcher`.

**Verified on FreeBSD (2026-11-04, 16-CURRENT arm64 in the cloud VM, QEMU TCG).**
`scripts/navigator-fetch-vm-check.sh` and stages 5 and 9 of
`scripts/navigator-converter-jail-e2e.sh`:
- The library's 23 tests, cross-built and run on FreeBSD: all pass. A test-harness race
  (the scripted server replied after one `read()`) failed about 6 runs in 7 on Linux; it
  was fixed in 9880f2b and then failed 0 runs in 30.
- `navigator-fetchd --gate` reports **M1 GATE: PASS** (23 rows) unjailed, as `navtest`
  under `cap_enter` + casper, and again **inside its jail**
  (`org.atrium.navigator.fetcher`, signed, `network = "full"`, system grant, daemon lane,
  own vnet). That includes `http://localhost/` resolved by casper (to `127.0.0.1`
  unjailed and `::1` in the jail), the grant control arm, and metadata refused even
  under the grant.
- **In serve mode, inside the jail:**
  - Admitted: a public literal, a casper-resolved name, plain http, and `example.com`.
  - Refused, with the range named: `127.0.0.1`, `localhost`, `10.0.0.5`,
    `169.254.169.254`, `100.64.0.1` (the jail's own epair gateway) and `[::1]`.
  - Refused at the hop: redirects from a public page to `10.0.0.5`, `localhost` and
    the metadata endpoint. Each probe keeps the scheme. The harness's first version
    redirected https→http, so it only proved the downgrade refusal.
- **Control.** With `--grant-private-network`, the same fetchd reaches a real listener
  on the guest's `127.0.0.1:80`. So the refusals come from the policy, not from an
  empty port.
- **Caveat: the "public" server is a stand-in.** The cloud container has no direct route
  out, so slirp's network was moved to a public range. A host-side server answers at
  `1.2.3.100` with a throwaway CA, and `/etc/hosts` names it `example.com`. The fetcher
  is unchanged. Rebinding on a live resolver was not exercised in the VM; the pinning
  tests cover it, and they pass on FreeBSD.

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

**Built (2026-11-05); verified on FreeBSD (2026-11-04, cloud VM).** Backend §4.7d has the
details and the numbers.
- **The mechanism.** The converter runs as a one-shot jail entry,
  `org.atrium.navigator.converter`, under `portcullis exec --daemon`, with a signed
  manifest that declares no capabilities. It takes no arguments and reads no
  environment; the job arrives on stdin. The fetcher is relayed over stdin/stdout,
  because the daemon lane passes exactly three descriptors. Blobs come back over the
  pipe and are hash-checked by the parent, and so does the recording.
- **The switch.** `PRERENDER_REQUIRE_CONFINEMENT=1` refuses three things: running
  without `PRERENDER_LAUNCHER`, running networked without `PRERENDER_FETCHD`, and a
  child the kernel does not report as jailed (`security.jail.jailed`). The child asks
  the kernel itself, so a launcher that silently fails to jail is caught.
- **Unconfined mode** prints `confinement: none — UNCONFINED, for testing`.
- **Verified on macOS, unconfined.** Serve mode reproduces the old in-child conversions
  byte for byte: 50/50 recordings offline, and 8/8 recordings plus 393 blobs with the
  network on.
- **Verified on FreeBSD** with `scripts/navigator-converter-jail-e2e.sh --rounds 2`:
  - Pages: 50, offline, with exploration on.
  - Jailed and unconfined conversions are **byte-identical**: 50/50 recordings and the
    reports, in both rounds.
  - An unconfined run with the require switch on is refused.
  - A networked page went through a jailed converter *and* a jailed fetchd. Its
    script was fetched and ran, and the recording is identical to the unconfined
    one.
  - No jails (live or dying), jaild zombies, mounts or upper dirs were left behind.
  - Jail cost per conversion, under TCG, is **about 3.06 s**. The numbers and why
    that is not the real figure are in §4.7d.
- **P2 limits, built 2026-11-05 (portcullis.md §6.5.2e′); verified on FreeBSD
  2026-11-04.** The VM numbers and one bug fixed are in §6.5.2e′. jaild creates the converter jail with rctl `memoryuse:sigkill=4096M` and
  `pcpu:deny=100`, from the manifest's `[resources]`. The tighter of manifest and caller
  wins, the limits are forwarded through `--daemon`, and the rules are removed with the
  jail. memfed budgets the converter jails as one pool (`[resources.pool]`) without
  exceeding that ceiling. As defence in depth, the converter still lowers its own
  **hard** `RLIMIT_CPU` (deadline + 5 s) and, on FreeBSD, `RLIMIT_AS`
  (`PRERENDER_MEM_MB`, default 4096) before parsing a byte. It reads them back and
  refuses on a mismatch. The per-conversion total byte budget is built (below).
  **The rlimit half, verified on FreeBSD (in the jail):**
  - `procstat -l` on the jailed `prerender` (uid `navtest`) shows hard and soft
    `cputime` 45 s (deadline 40 + 5) and `vmemoryuse` 1 048 576 KB
    (`PRERENDER_MEM_MB=1024`).
  - The memory limit is enforced. A page that grows a 2^27-character string fails
    at `PRERENDER_MEM_MB=256` ("memory allocation of 134217792 bytes failed", no
    recording). It converts at 1024.
  - A broker whose hard limit was already below the ask made the child refuse:
    `RLIMIT_AS` and `RLIMIT_CPU` each failed with EPERM, exit 3. Fixed (2026-11-05):
    the child now takes the **tighter** of the ask and the inherited hard limit, and
    still reads the result back.
    Re-checked in the VM: with the same lowered hard `RLIMIT_AS` and `RLIMIT_CPU`, the
    child now converts.
  **The rctl half, verified on FreeBSD:**
  - The converter jail carries `memoryuse:sigkill=4294967296` and `pcpu:deny=100`
    while it runs. The worker jail carries 2 GiB and 100.
  - Every rule leaves with its jail.
  - `--memory 256` kills a page that grows a 256 MiB string.
  - memfed tightens and loosens the pool members, clamped at the 4 GiB ceiling. The
    loosening needed a fix: `SetRctl` had been stacking rules.
  - The CPU limit is a backstop behind the wall-clock deadline, which fires first.
- **Per-conversion fetched-bytes budget, built 2026-11-05.**
  - *Where.* The budget is enforced in the driver's fetch relay
    (`FetchBudget`, navigator-prerender `fetch.rs`, used by `convert_in_child`). That
    is the trusted side of the jail: the converter only asks, so a compromised
    converter cannot spend past it.
  - *What counts.* Every byte pulled from upstream, delivered or not.
  - *When it is spent.* Each later request gets an `err` frame without reaching
    upstream. The response that crosses the limit is pulled (fetchd already bounds it
    to 8 MiB) and refused, not delivered.
  - *Bound.* The converter receives at most the budget. Upstream is asked for at most
    the budget plus one response.
  - *Reporting.* The batch report prints the relayed-bytes distribution and names
    each document the budget refused, with bytes pulled, the limit, the number of
    requests refused and the first refused URL.
  - *Default: 128 MiB* (`PRERENDER_FETCH_BUDGET` overrides it, in bytes). Chosen from
    the relayed bytes of both corpora's 123 conversions, 2026-11-05: p50 1.5 MiB, p90
    8.4 MiB, p99 26.7 MiB, max 75.6 MiB (theverge, 295 requests). It admits every
    conversion measured, with 1.7x headroom over the largest; 64 MiB would have
    refused theverge.
  - *Checked on theverge.* The default admits it (75.6 MiB). At 32 MiB the report
    reads "fetch budget exhausted: 33577647 bytes pulled of a 33554432-byte limit;
    112 request(s) refused". The fetchd per-response limit is unchanged.
  - *Tests.* Unit tests: delivered up to the limit, the crossing response refused,
    upstream not asked afterwards, errors cost nothing. End to end through the driver
    with a fake fetchd (`tests/confine.rs`): five 100 KB scripts against 250 KB
    deliver two, and upstream sees three requests.
    It was not tripped on its own.

### 2.3 Normalize and render — the document worker

**Enforced today.**
- One worker process per document, with a pipe as its only channel.
- The pipe is read as untrusted, with bounded frames, deadlines, and retirement on a lie.
- The Document Profile ceilings are refused at ingest.
- The validator is fuzzed with reached-coverage asserted (§4.7b).
- Under `Confinement::Launcher` the worker runs in a signed, zero-capability jail (measured
  in §4.7a).
  `navigator-jail-e2e.sh --quick` passed again on FreeBSD on 2026-11-04 (the cloud VM),
  covering refusals and leak checks, alongside the §2.2 run.

**Gap — confinement is opt-in.** `JailedHost` jails only when built with a launcher.
`unconfined_for_testing` is honestly named and `require_confinement()` exists, but nothing
yet *calls* `require_confinement()` on the production path. **Fix (P1):** the shipped
`navigatord` refuses to start without a launcher; unconfined mode is a test-build feature.

**What a compromised worker gets.** Nothing on the machine. Its only output is a scene
graph, which the broker validates for size and shape. §10.5 admits that the *content* of a
valid scene graph is a steganographic channel. That channel leads only to the compositor
and the screen, so it matters only when combined with a compositor bug (§2.5).

**Rating:** today medium (an unjailed default), designed **low**.

**Built (2026-11-05).** `Navigatord::start(host)` refuses a host that does not report
`is_confined()`. `DocumentHost::is_confined` defaults to false, so a new host must claim
confinement to get it. The unconfined constructor, `Navigatord::unconfined_for_testing`,
is compiled only under `cfg(test)` or the `unconfined-for-testing` feature, which the
crate's own dev-dependency turns on for its tests and examples. A probe crate that
depends on `navigator-backend` without the feature fails to compile a call to it
(`E0599`). `jailed_corpus` uses `start` whenever a launcher is configured.

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

#### 2.5.1 Settled: the compositor writes data, the broker runs only our kernels

*Settled (user, 2026-11-04). This replaces an earlier draft in which the jailed compositor
held a narrowed client `device_fd`. That draft left the driver's client ioctls reachable
from the process that parses hostile scene graphs, so the user asked why and we redesigned.*

**Why a GPU-submitting compositor can't be fully jailed.** A process that submits GPU work
has to create buffers, bind them into a GPU VM and submit command buffers. Every one of
those is an ioctl into our kernel driver (GPU ABI v2.1 §4.1, §5.12). A jail removes
everything *around* that, but not the one thing the process exists to do.

**The shape that removes it.** frescod's GPU path doesn't run arbitrary shaders. It runs
two fixed kernels of our own, `bin` and `raster` (`fresco-compute`), over buffers staged in
one System BO. So the compositor never needs to submit anything:

| process | holds | parses hostile input |
|---|---|---|
| **GPU/display broker** (small, no parser) | `control_fd`, its own client context, the display node; our signed `bin`/`raster` kernels | **nothing**: it checks only the size and header of the shared scene buffer |
| **Fresco compositor** (jailed: zero filesystem, zero network, **no GPU fd**) | a pre-mapped shared scene buffer per frame slot, from the broker; its client sockets; the shipped font set mapped read-only | scene graphs, glyph outlines, pixel buffers |

Per frame:
1. The compositor writes the frame's scene into a shared buffer: rects, paths, glyph
   outlines and image pixels.
2. It signals the broker over its socket.
3. The broker submits `bin`, then `raster`, with that buffer bound read-only and the
   scanout BO as the target.
4. The broker flips the result.

The compositor makes **zero driver ioctls**.

**What remains to attack, and why it is small.**
- **Our two kernels reading hostile data.** Every load and store is bounds-checked against
  the bound buffer's size. A count or offset in the scene is never trusted as an address.
  The GPU's page tables confine the kernels to the BOs the broker bound, which is
  `gpu-isolation.md`'s invariant, attested.
  - The kernels are code we wrote, and they have a CPU reference (`fresco-compute`'s Rust
    reference, already pixel-exact against Tier-2 and the R9700).
  - So they can be fuzzed differentially: random scene buffers, GPU output against the
    reference, plus out-of-bounds detection under gpusim.
- **A GPU hang from a pathological scene** (huge path counts, degenerate geometry) costs a
  queue reset, not the machine (GPU ABI §10.1–10.3). The broker also bounds per-frame work
  from the buffer header before it submits.
- **The broker's small input.** It reads a fixed header (sizes, counts) and a doorbell
  message from the socket, and nothing else.

**What still holds from the earlier draft.** The compositor sees every window's pixels and
routes input, because that is its job.
- **Secure input:** keystrokes for password fields go from the UI to the broker's request
  builder (§6.4), never through the compositor.
- **Trusted chrome:** the origin indicator is drawn by the broker as a separate overlay
  above the compositor's output, so a compromised compositor can't paint a fake origin.

**The cost, accepted.** The compositor can use the GPU only for what `bin` and `raster`
do. For a 2D compositor that is the whole job, and it is already the compute route chosen
for the display pipeline. Fonts follow §5 question 4: web fonts arrive as outlines.

**Apps that need their own GPU work were out of scope here.** §2.5.3 now proposes how they
fit the same per-window shape. Until then, today's rule stands: apps hold no `device_fd` in
production, and they reach the GPU only through the broker's aqueduct-gpu protocol.

**Rating:** low, bounded by two fuzzable kernels and a fixed-size header.

**Also P2.**
- Fuzz the compositor's scene ingest.
- Differential-fuzz `bin`/`raster` against the CPU reference.
- Remove the GPU device node from the compositor's jail entirely, through its devfs
  ruleset.

#### 2.5.2 Proposed: one jailed renderer per window, no process that sees two

*Proposed 2026-11-04, from the user's suggestion ("if each window had a separate jailed
compositor which maps only the portion of the shared memory that it would show the
window"). It removes §2.5.1's last limitation: a compositor that sees every window.*

**The split goes one step further.** "Compositor" has been two jobs in one process.

1. **Rendering a window's content.** This means consuming that window's scene graph,
   which is hostile. It is per window by nature.
2. **Composing windows into a screen.** This means stacking, positioning, clipping,
   opacity and scrolling. Its input is a short layer list that the window manager decides,
   not content.

Job 1 needs a parser and job 2 needs to see everything. Neither needs the other.

| process | maps | sees |
|---|---|---|
| **window renderer** (one jail per window, scoped to that window's origin) | **only its own** scene buffer, one shm object the broker passed it | its own window's content and input events. It never sees pixels, not even its own |
| **broker** (no parser) | every window's surface BO, on the GPU side only | a layer list: surface, rect, z, clip, opacity. All of it is trusted window-manager state |

**Per frame.**
1. Each window renderer whose window changed writes its scene buffer and signals the
   broker.
2. For each such window, the broker dispatches `bin`/`raster` with **that window's** scene
   buffer as input and **that window's** surface BO as the only writable target.
3. The broker runs a fixed `compose` kernel of ours over the surfaces, using the layer
   list.
4. Trusted chrome (§2.5.1) goes on top, and the broker flips.

**One GPU VM per window.** GPU ABI v2.1 already allows one VM per app inside the broker's
context. Each window's dispatches run in a VM that contains only that window's scene buffer
and surface. If a bounds check in `bin`/`raster` were ever wrong, the GPU's page tables still
stop the write at the edge of that window's memory. Only the `compose` pass runs in a VM
that can read every surface. Its input is the trusted layer list, and it never reads a
hostile buffer.

**Input splits the same way.** The broker routes each event by the layer list (which
window is under the pointer, which has focus). A window's renderer receives only its own
events, already translated to window coordinates. It can't observe keystrokes or pointer
motion meant for another window.

**What this buys.** A compromised renderer for window A reaches A's content and A's
input, which origin A already had. Cross-window exposure is zero by construction, not by
validation. This is the same argument as one jail per document (backend §2), carried all
the way to the screen.

**It can share a jail with the document worker.** A window's renderer and the document
worker for that window are the same trust domain: one origin, one document. Merging them
saves a jail and a copy. Keeping them apart keeps the worker's output passing the §4.2
validator, which semantics and accessibility still need. *Recommend: separate at first,
because the validator is already built. Merge only if the measured cost says so.*

**It stays a scene-graph compositor, not a Wayland-style one.** *(User question, 2026-11-04.)*
In Wayland the client renders its own pixels and the compositor only stacks buffers. Here:
- **The app never renders.** It sends a scene graph, as Fresco was designed for. The
  per-window renderer is *system* code (today's Fresco scene renderer, instantiated per
  window), not app code, and the pixels come from our kernels.
- **The surface is a cache, not the app's output.** The system can re-render a window from
  its retained scene at any scale, theme or text size, for example for zoom or an overview,
  without asking the app.
- **Semantics survive.** Each renderer exports its window's semantic tree (accessibility,
  text selection, search) to the services that consume it, over a validated channel. It is
  the same data as before, now one tree per window instead of one global tree.
- **What changes is that no global scene exists in one address space.** System features
  that need several windows' *content* (cross-window search, a full accessibility view) go
  through those per-window exports rather than reading a shared scene. Glyph atlases and
  other caches are per window, which costs some memory. A shared cache would be §7.3's
  side channel anyway.
- **The Wayland-like exception is deliberate.** GPU-heavy apps (§2.5.3) do render their
  own buffers, because their shaders are their content. They are the exception, and
  ordinary apps and the Navigator stay scene-graph clients.

**Costs and limits, stated.**
- **Memory.** One surface BO per window. Every modern compositor already keeps a backing
  store per window (macOS, Wayland), so this is the normal cost, not a new one.
- **GPU work.** One `bin`/`raster` pair per *damaged* window per frame, plus one `compose`.
  Idle windows reuse their surface.
- **Cross-window effects** (backdrop blur, a shadow over the window below) can only be
  fixed effects inside `compose`, driven by the layer list. An app can't supply its own
  cross-window shader. That is the point.
- **Embedded surfaces** (Limen, §8.1 islands) are just more layers. An island's renderer is
  its own jail, and the document around it never maps the island's buffer. That is the
  structural answer to the island-overlay clickjacking of §2.9: the broker knows exactly
  which layer is on top.
- **Side channels.** Windows share the GPU's time and caches, so contention timing between
  windows remains possible. It is low bandwidth, and per-window scheduling weights (GPU ABI
  §4.2) are the lever if it ever matters.

**Rating:** with §2.5.1 alone, low but cross-window. With this, low and per-window.

#### 2.5.3 Proposed: apps with their own GPU work use the same per-window shape

*Proposed 2026-11-04, from the user's question: "this can also solve the gpu work for apps
if each app gets its own jailed renderer right?"*

**Mostly yes.** §2.5.2's structure is exactly the isolation an app's own GPU work needs:
- one GPU VM per app, holding only that app's buffers;
- one surface it alone can write;
- composition by a trusted layer list;
- input routed by the broker.

The one thing that changes is **whose code runs on the GPU**. A Navigator window runs only
our `bin`/`raster`. A 3D app, a game, an ML job or Orbis runs *its own* shaders. That moves
the residual risk to three places, each with an owner:

1. **The shader compiler is a hostile-input parser.** It translates SPIR-V into RDNA4
   machine code (`atrium-spv-*`, the GIR pipeline).
   - **It runs in the app's own jail**, never in the broker.
   - The broker receives machine code plus the aqueduct-gpu §11 validation record:
     bounded loops, descriptor coverage, interface checks.
   - The broker never compiles.
   - *Decided 2026-11-05 (option A, `gpu-app-path-f6.md`):* the app controls the record as
     much as the code, so the broker treats both as untrusted. It checks only the code header
     against hardware limits and writes every packet itself. The record is advisory.
2. **Arbitrary machine code on the GPU is confined by the GPU MMU, not by inspection.**
   An app's dispatches run under its own VMID with page tables the kernel built. That is
   `gpu-isolation.md`'s invariant, and the app GPU path stays gated on its attestation.
   - Machine code cannot program page tables or privileged registers.
   - Proving the driver never lets a command-buffer packet do so is the kernel's job
     (GPU ABI §5.12).
3. **Submission stays in the broker.** The app builds a command stream in its own shared
   buffer, using aqueduct-gpu's closed frame vocabulary (§3). The broker submits it into the
   app's VM, with:
   - the app's quota and scheduling weight (GPU ABI §4.2);
   - a per-dispatch timeout, so a hang costs that app a queue reset (§10.1).

   The app holds **no `device_fd`**, so the production rule "apps hold no GPU fds" survives
   unchanged.

**What this does not solve, stated.**
- **Shared-hardware side channels between apps:** cache and timing contention. Mitigated
  by scheduling, not eliminated. The same holds on every GPU OS.
- **Performance-sensitive apps** may want user-mode queues (GPU ABI §5.9). A doorbell
  mapped into an app jail is a direct path to the GPU. Under this design it stays test-only
  until a separate review says otherwise.
- **The broker's frame-vocabulary parser** grows with app needs. It must stay closed and
  fuzzed, because it is the one place the broker reads app-written data.

**Net effect.** The Navigator, native 2D apps and GPU-heavy apps all share one shape:
- a renderer per window or app, in its own jail and its own GPU VM;
- a broker that parses no content;
- a compose pass over a trusted layer list.

The difference between them is only which kernels the broker is willing to run in that
VM: ours alone, or the app's validated ones.

#### 2.5.4 What must be true before §2.5.1–2.5.3 secure anything

*Recorded 2026-11-04 at the user's request. The architecture is sound on paper, but none of
it is built: today frescod is still one process holding the device, the display and every
window's scene. The design is only as strong as these conditions, each of which has an
owner.*

| # | condition | why the design depends on it | owner |
|---|---|---|---|
| **F1** | **The kernel GPU driver enforces per-VM isolation** and passes `gpu-isolation.md`'s attestation gate | Per-window and per-app GPU VMs (§2.5.2, §2.5.3) are only real if the driver enforces them. The 2026-11-03 review (`docs/reviews/2026-11-03-gpu-kernel-and-tier2-review.md` §A3) lists five breaks in `atrium-gpu-amd`: (1) unlocked shared-queue reprogramming runs one ring under another VMID; (2) BO and VM teardown while in flight; (3) doorbell mmap and `QUEUE_MAP` hijack; (4) scanout by a forgeable VRAM offset (display ABI v2's commit-by-BO-fd, b3d298e, may already close this; to be confirmed); (5) unprivileged reset, scheduler and power control. **This is the largest item and blocks everything else.** gpusim's synchronous drain hides 1 and 2, so they need asynchronous tests. | GPU driver |
| **F2** | **`bin`, `raster` and `compose` never read or write outside their bound buffers**, for any input | They are the only code that reads hostile scene data on the GPU. Every count and offset is checked against the buffer's size, and per-frame work is bounded from the header. Verified by **differential fuzzing** against `fresco-compute`'s CPU reference (random and mutated scene buffers; GPU output must match, and gpusim must report no out-of-bounds access), with reached-coverage asserted, as in backend §4.7b. | Fresco, with the RDNA4 compiler for gpusim OOB detection |
| **F3** | **The broker stays small, parses only fixed formats, and is fuzzed** | It holds the control fd, the display and every surface. It reads: the scene-buffer header, the per-window wake-up message, and the layer list. The window-manager policy that *writes* the layer list (stacking, focus, placement) must live in the broker or be equally trusted, because a forged layer list is a cross-window attack. | Fresco |
| **F4** | **The split is built**: per-window jailed renderers with no GPU fd; per-window shm and GPU VM; the broker's `compose` pass; input routed by the layer list; trusted chrome as a broker overlay; the password secure-input path (§6.4) | This is §2.5.1–2.5.2 itself. Until it exists, a frescod compromise is a whole-desktop compromise. | Fresco |
| **F5** | **New channels are validated like the scene graph** | Each renderer's semantic-tree export (accessibility, selection, search) is renderer output that other services parse. It gets bounded frames, hard limits and fuzzing, as backend §4.2/§4.7b require for scene graphs. | Fresco, plus each consuming service |
| **F6** | **The app GPU path (§2.5.3) stays gated** on F1 and on the aqueduct-gpu §11 validation, with the shader compiler running in the app's jail | Apps run their own machine code. Only the GPU MMU confines it. | GPU driver, aqueduct-gpu |

**Open by nature, not by omission.** Windows and apps share the GPU's time and caches, so
contention timing between them remains a low-bandwidth side channel. Per-window scheduling
weights (GPU ABI §4.2) are the lever, as on every GPU operating system.

**Order.**
1. F1, starting with breaks 1–3, which matter as soon as two contexts exist.
2. F4 and F2 together: the split is only safe once the kernels are fuzzed.
3. F3 and F5 alongside F4.
4. F6 last, because nothing uses it until apps bring their own shaders.

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

### 2.8 Credentials and logins

There are none today, which is why A1 currently yields no account takeover. Once logins
exist, credentials are the asset an attacker most wants, so they get their own section:
**§6**. The short version: no login support ships before §6's vault and per-origin fetcher
exist.

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
| **P0** ✔ built, verified on FreeBSD | Jail the converter process (Boa) with zero capabilities via `portcullis exec` | A1 → code execution as the user | small: the launcher exists and was measured (§4.7a) | §2.2 |
| P1 ✔ built, verified on FreeBSD | Refuse non-global addresses after resolution; pin the address per conversion; intranet as an explicit grant | A4: LAN and localhost, DNS rebinding | small | §2.1 |
| P1 ✔ built | `navigatord` refuses to start unconfined outside test builds | A1 against the worker | trivial | §2.3 |
| P1 | Decide where images decode: per-document jail, memory-safe decoders, never in Fresco | A1 via decoders | design and medium code | §2.4 |
| P1 | Per-user dedup domain by default; reproduce-to-share | A5 poisoning | design now, code with sharing | §2.6 |
| P2 ✔ rctl built and verified on FreeBSD; ✔ byte budget built (128 MiB per conversion, in the relay) | `rctl` memory/CPU limits on converter and worker jails; per-conversion byte budget | A6 | small | §2.2 |
| **P0 (graphics)** | Fix the five GPU driver isolation breaks and pass the attestation gate (F1) | everything in §2.5 rests on it | medium–large | §2.5.4 |
| P1 | Split frescod: jailed compositor with **no GPU fd** writes a shared scene buffer; the broker runs only our `bin`/`raster` kernels; secure-input path | A1 against the shared compositor and the driver | medium | §2.5.1, conditions F2–F5 in §2.5.4 |
| P2 | One jailed renderer per window, one GPU VM per window, fixed `compose` kernel; input routed by layer list (§2.5.2) | cross-window exposure | medium | §2.5.2 |
| P2 | Fuzz Fresco scene ingest and the GPU ioctl surface; Fresco takes outlines, not fonts | A1 against the shared compositor | medium | §2.5 |
| P2 | Broker rules for replayed request templates | A3, CSRF | design-time, with §5.4.5 | §2.7 |
| P2 | Unobscured-input rule for islands; bhyve for the CDM | A2 clickjacking, kernel surface | medium, with §8.1 | §2.9 |
| P2 | Scheme allowlist on link activation; download marking | A1/A2 | small | §2.10 |
| P3 | Origin chrome, punycode for confusables, form warnings | A2 | product work | §2.11 |
| gate | No login support before §6's vault and per-origin fetcher (C0) | account takeover | — | §6 |

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

*Recommendations accepted by the user, 2026-11-04.*

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

## 6. Credentials: cookies, sessions, tokens, SSO

*Proposed 2026-11-04 at the user's request ("these are the ones we need to pay particular
attention to"). None of it is built: the Navigator has no login support today.*

A stolen session cookie or token is worth more to an attacker than code execution on most
machines. It works from anywhere, for as long as the session lives, and it never trips a
jail monitor. So the design goal is not only "a compromise cannot steal credentials". It is
stronger: **no process that parses hostile input ever holds a credential in readable form.**

### 6.1 What counts as a credential

| kind | where it lives on the web | what an attacker does with it |
|---|---|---|
| session cookies (often `HttpOnly`) | cookie jar | replays the session from their own machine |
| persistent "remember me" cookies | cookie jar | the same, for weeks |
| bearer tokens, OAuth access and refresh tokens | page JS memory, `localStorage`, `sessionStorage` | calls the site's API as the user; a refresh token mints new ones indefinitely |
| CSRF tokens | in the page's HTML or JS | forges a state-changing request |
| passwords | typed into forms; the password manager | everything above, on every site that reuses it |
| SSO / IdP sessions (Google, Microsoft, Okta) | the IdP's own cookies | logs in as the user to *every* relying party |
| passkeys / WebAuthn private keys | an authenticator | nothing, if the key never leaves the authenticator |
| client certificates | key store | impersonation at the TLS layer |

The SSO row is the reason this is the highest-value asset class. One IdP cookie can be
equivalent to all of the user's work accounts.

### 6.2 The rule: credentials are attached, never handed over

> **A credential is attached to a request by the fetcher, at send time, for exactly the
> origin that owns it. No parser, converter, document worker, island engine or compositor
> ever receives one.**

Today's code already points this way. The fetcher strips Cookie, Referer and Authorization
by construction (§2.1), and conversions are anonymous (§5.4.3 invariant 1). The rule turns
"anonymous" into "credentialed, but with the credentials held where hostile code cannot
read them".

**The components** (extending backend §2):

| component | holds | never holds |
|---|---|---|
| **credential vault** (new; no network, no parser) | the cookie jars, token store and password store, encrypted at rest, partitioned by origin | page content |
| **per-origin fetcher** (§7.7, made concrete) | for the life of one request: the jar slice for that one origin, handed over by the vault | any other origin's credentials; any file |
| converter, document worker, island engine | response bodies | `Set-Cookie` values, `Authorization` headers, the jar |
| broker (`navigatord`) | which origins have a session (policy) | credential values |

**How a request flows.**
1. The worker or engine asks the fetcher for `GET https://mail.example/inbox`.
2. The fetcher asks the vault for `mail.example`'s cookies matching that request: path,
   `Secure`, `SameSite`, expiry.
3. The vault answers, and the fetcher sends the request.
4. On response, the fetcher **strips `Set-Cookie`** before passing the body on, and hands
   the new cookies to the vault.
5. The body reaches the parser. The cookies never do.

**This is stronger than browsers.** In a mainstream browser, the renderer of an origin is
trusted with that origin's non-`HttpOnly` cookies, and a renderer exploit commonly reaches
the network stack's cookie store. Here even a compromised tier-4 engine **cannot read** the
session cookie. It can only *ride* the session through the fetcher while it is alive, and
§6.6 bounds that.

### 6.3 Partitioning

- **The partition key is (top-level site, origin).** An origin embedded under a different
  top-level site gets a separate jar. That is CHIPS-style partitioning, applied to all
  cookies.
- **No third-party cookies, ever.** Partitioning makes them useless for tracking (A3), and
  this project has no compatibility debt that would require them (§7.6's argument).
- **`SameSite` defaults to `Lax`** when a site sets none, as current browsers do.
- **`Secure` and `__Host-` prefixes are enforced.** A cookie from an `http:` origin can
  never shadow an `https:` one.
- **IdP cookies are first-party at the IdP.** A top-level navigation to
  `accounts.google.com` uses that origin's own jar, so ordinary redirect-based SSO works
  (§6.5).

### 6.4 Logging in: passwords never enter a parser

A login form is rendered by the document worker, but the **values typed into a password
field must never reach it**.

- The UI recognises a password field from the scene graph's semantic tree. The field is
  marked by the worker, but the broker validates the mark: the field must sit inside a
  form whose action is same-origin and `https`.
- Keystrokes for that field go from the UI **to the broker's request builder**, not to the
  worker. That is the secure-input path of §2.5.1. The worker sees only bullets.
- Submission is a **user-initiated navigation**: a POST built by the broker from the form's
  declared fields and sent through the per-origin fetcher. It is not the page network,
  which stays GET-only (§5.4.1c).
- **Password manager.** Autofill matches the exact origin, never the registrable domain
  alone, and fills only on a user gesture. It refuses on `http:`, on a confusable domain
  (§2.11) and inside a tier-4 island unless the user confirms.
- **Passkeys first.** WebAuthn is the one credential that phishing cannot steal, because
  the authenticator checks the origin itself. The broker implements the WebAuthn client:
  - the RP ID check happens in trusted code, against the origin the fetcher actually
    connected to;
  - the private key stays in a platform authenticator, a separate process with no network
    and no parser, or a hardware key;
  - pages can request a credential, never read one.

### 6.5 SSO, OAuth and OIDC

The common flows, and what each needs:

| flow | how it works | what the Navigator needs |
|---|---|---|
| redirect (OAuth code, SAML redirect, OIDC) | app → IdP → app with a `code` | top-level navigations across origins, each using its own jar: **works under §6.3** |
| `form_post` / SAML POST | IdP returns an auto-submitting form | a self-submitting form with no JS. Recognise the pattern (a single form, hidden fields, an `onload` submit) and ask the user to continue, or allow it once the user has initiated login on that IdP |
| popup login | `window.open` to the IdP, `postMessage` back | tier 4 only. In the document lane, rewrite it as a redirect where the IdP supports one (most do) |
| silent renew in a hidden iframe (`prompt=none`) | the IdP iframe reads its own cookie under the app | **broken by partitioning**, as it already is under Safari ITP and Firefox TCP. Apps fall back to refresh tokens or a redirect. FedCM is the designed replacement; implement FedCM in the broker when tier 4 needs it |
| device-bound sessions (DBSC) | the server binds a session to a key the device holds | **support it**: the key lives in the vault's keystore (or the TPM), so a stolen cookie is useless off this machine |

**Tokens held in JS** (SPAs keeping an access token in memory or `localStorage`) are where
the §6.2 rule bends. The page's own code reads the token and attaches it, so the engine
sees it.
- **Document lane with data bindings (§5.4.5).** A binding's request template carries a
  credential *slot*, not a value: `Authorization: Bearer {vault:mail.example/api}`. The
  fetcher fills the slot. The converter records the token's *shape and source* (which
  response set it, which header used it), never the value. Where that works, the token
  never sits in a document.
- **Tier 4.** The engine holds the token. That is the honest exposure of the legacy lane
  (§7.4). It is bounded by §6.6: one origin, a short-lived jail, and storage that persists
  only through the vault, encrypted.

### 6.6 Bounding what a compromise can do with a session

A compromised engine for `mail.example` cannot read `HttpOnly` cookies, but it can make
requests that carry them.

- **Origin-locked fetcher.** A request outside the jail's own origin never carries any
  credential, and is reported to the monitor (§10.5). That rule is exact, so it can
  terminate rather than just warn.
- **Session recycling (§11.9.1)** shortens the window in which a compromised engine can
  ride the session.
- **Rate and shape limits per origin.** For example, at most N state-changing requests a
  minute without a user gesture. Mass deletion or exfiltration through the session then
  becomes slow and visible.
- **Step-up on sensitive actions** is the site's job, but passkeys (§6.4) make it cheap
  for sites to ask.

### 6.7 Storage, lifetime and clearing

- The vault store is **encrypted at rest** with a key from the user's login keychain. It
  is never in the CAS, never deduplicated and never shared (§2.6).
- **Session cookies** (no `Expires`) die when the user closes the origin's last view, not
  at process restart. Restoring sessions is an explicit setting.
- **Per-origin clearing** is one action. "Log out everywhere" clears all jars and revokes
  DBSC keys.
- **Credentialed conversions are private.** A page fetched with credentials produces a
  conversion that is per-user, tagged as credentialed, and never shareable. It is deleted
  when that origin's jar is cleared. This closes the gap where a logged-in inbox ends up in
  a deduplicated store.
- **No sync** of jars or tokens without an explicit, per-destination user decision. The
  same rule as the monitor's history (§10.6).

### 6.8 Ranked work for credentials

| # | work | gate for |
|---|---|---|
| C0 | Credential vault process plus per-origin fetcher; the fetcher strips `Set-Cookie` and attaches from the vault | **any** login support |
| C0 | Credentialed conversions are private and unshareable | any login support |
| C1 | Secure-input path for password fields; broker-built form POST | password login |
| C1 | `(top-level site, origin)` partitioning; no third-party cookies; `Secure`, `SameSite=Lax` and prefix enforcement | any login support |
| C1 | WebAuthn client in the broker plus a platform authenticator | phishing-resistant login |
| C2 | Credential slots in §5.4.5 request templates | SPA data bindings with auth |
| C2 | Origin-locked fetcher with monitor termination; per-origin request-rate limits | tier-4 login |
| C2 | DBSC support | stolen-cookie resistance |
| C3 | FedCM; `form_post` continuation | the remaining SSO flows |

**Open question.** Is the vault its own process, or a mode of `navigatord`? *Recommend: its
own process.* `navigatord` holds the store and jail-spawn authority. Adding credentials
would make it the single highest-value process on the machine. Two small processes with
one job each are easier to reason about, and to fuzz, than one that holds everything.
