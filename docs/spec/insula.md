# Insula — third-party application platform

Status: design sketch (pre-D-phase). Not yet on the roadmap.
Last updated: 2026-05-21.

**Insula** is Atrium's app-platform layer: the contract,
manifest conventions, and supporting services that third-
party applications target. The Atrium metaphor extends
naturally — *Atrium* is the central courtyard of the Roman
townhouse (the platform's core), and *insulae* were the
multi-unit apartment buildings filling the rest of the city
(where the population — third-party apps — lives, each in
its own walled-off unit).

Insula is a layer, not a single daemon. It comprises:

- A **contract** — what apps see when targeting Atrium
  (manifest extensions, syscall allowlist, `libatrium.so`
  ABI, capability vocabulary, embed roles, addressing).
- **New services** introduced specifically for the app-
  platform role: **Limen** (cross-jail composition),
  **Tabellarius** (push delivery), **Loculus** (wallet /
  autofill), **Concursus** (peer-to-peer channels),
  **Nomenclator** (name resolution).
- **Existing-service conventions** for app-platform use:
  Portcullis manifests, Vestibulum per-app sign-in,
  Praeco delivery, Opifex bundles, Scrinium pickers, Tabula
  clipboard.

This document covers what the web calls "client-side
scripting" but executed under Atrium's kernel-enforced
sandbox rather than an in-process browser runtime. The
premise: with Portcullis providing kernel-level isolation,
we do not need an interpreted/verified bytecode runtime in
the V8/WASM-sandbox sense. Apps ship as native binaries
(or as portable IR for cross-arch), execute in jails, and
reach the system only through capability-gated Aqueduct
services.

## 0. Position relative to existing technology

The browser web stack solves a specific problem: run untrusted
code from any URL in a shared address space (the renderer
process) without compromising the host. Every constraint
follows from that — JS / WASM as verifiable bytecode, same-
origin policy, CSP, the DOM-as-platform-API, the permission
prompt UX, the lack of real syscalls. V8 *is* the sandbox, so
V8 owns the language, the memory model, and the API surface.

Atrium does not have that constraint. The sandbox is a
FreeBSD jail managed by Portcullis. The kernel + MMU + jail
infrastructure enforce isolation; the code on the inside can
be arbitrary native ELF. This collapses the whole web-platform
stack to its first-principles shape:

- Distribution: signed, content-addressed bundle in Tessera.
- Execution: native ELF in a Portcullis jail.
- Sandbox: kernel-enforced, capability-shaped.
- API: a frozen C-ABI platform library + Aqueduct services.
- UI: native toolkit (Pergola), not a DOM.

WASM does not appear in this design. The browser needed it
because the sandbox was in-process; Atrium does not have that
problem. WASM may still be useful as a *distribution format*
for cross-arch portability (§3), but it is never the runtime.

## 0.5 Category collapse — "app" is the only category

A direct consequence of the design that is worth making
explicit, because readers from a web background will look
for distinctions that no longer exist.

### 0.5.1 What goes away

The web's three-way split — **native app / PWA / website** —
exists for historical reasons that do not apply to Insula:

| Web split | Why it existed | Insula's answer |
|---|---|---|
| Native vs. webapp | Native has full system access; webapps must be sandboxed | Both fully sandboxed by the same jail model; native speed for both |
| Webapp vs. website | Webapps are "installed" PWAs; websites are pages | Apps install via Opifex (§3); documents are content rendered by `atrium-doc` (§10.6) |
| App store vs. open web | Curated vs. uncurated distribution | One distribution mechanism — content-addressed signed bundles + Nomenclator names; the user picks any registry they trust |

In Insula there is **one category — "app"** — distinguished
only by what capabilities the manifest declares and the user
consented to at install time. A "weather widget," a "video
editor," and what would have been a "weather website" are
the same kind of thing: a Pergola app in a Portcullis jail.
Their differences live in the capability manifest, not the
deployment model.

### 0.5.2 What survives — documents are not apps

The clean separation is between **programs** (apps, which
are inert without execution) and **content** (documents,
which are inert without a viewer). Insula draws this line
sharply where the web blurred it:

- Apps run code. They require install consent because they
  receive capabilities.
- Documents are bytes. They receive no capabilities; the
  viewer (`atrium-doc`) renders them in a doubly-jailed
  inner context with zero ambient authority.

The casual-browsing UX the web was best at — "follow a
link, see content, follow another link" — survives
*completely*, for documents. Wikipedia, news, blogs,
papers, recipes, search results, READMEs: zero install
ceremony, just rendering. This covers the bulk of what
people actually casually "browse."

### 0.5.3 What changes — apps require an install moment

What the web *also* allowed — "follow a link, accidentally
end up running a webapp that accumulates trust" — is gone.
Apps require explicit install consent (the capability
manifest displayed for review). This is good from a safety
standpoint and is the explicit design intent.

For apps whose authors genuinely want frictionless
onboarding, a **trial-launch** pattern is supported:

- Manifest declares `[trial]` mode with a reduced
  capability set (typically: no persistent storage, no
  network beyond a declared host, time-limited session).
- The launcher offers "Try" alongside "Install."
- Trial mode runs in a jail with the reduced caps; expires
  after the declared session.
- User explicitly upgrades to install for the full manifest
  if they want to keep using the app.

This is the App Clip / Instant App pattern from mobile OSes,
made first-class. It preserves the "try without committing"
property while making the trust escalation a deliberate user
action.

### 0.5.4 Launcher implications

Forum (the dock / launcher) becomes the user's view of
**every program-shaped thing on the system**. There is no
separate "browser bookmarks," no "open recent websites" —
those concepts dissolve. Pinned items are apps. Recents are
apps and documents. The single mental model replaces a
half-dozen historically-accreted ones.

A user's URL-bar-equivalent is just a name input that goes
to Nomenclator. It does *not* magic-dispatch between
"search" and "navigate" — that conflation is a web
accident. **Search is its own app.** A user who wants to
search picks the search app (or its launcher integration);
a user who knows where they are going types the name. The
two are different actions with different UX, not a single
mystery box that sometimes guesses wrong.

## 0.6 The "web version" disappears

The most economically consequential consequence of §0.5.
Worth calling out explicitly because the entire SaaS
industry has organized itself around solving a problem that
disappears in this model.

### 0.6.1 What "web version" actually is

Every major desktop app has a parallel web version: Office
365 web, Photoshop Web, Figma (web-only), Slack and Discord
and Notion (all Electron — desktop apps shipping a browser
to run their own web app). These exist not because anyone
wanted a web app, but because the web was the lowest-
common-denominator delivery mechanism that solved four
distinct problems:

| Problem | Why the web solved it | Insula's answer |
|---|---|---|
| Cross-platform delivery | Browser is universal | Aqueduct + the Insula contract are OS-agnostic by design; one codebase runs on every Atrium device |
| Zero install friction | Just visit a URL | Opifex install is one consent prompt (§3, §14); trial-launch (§0.5.3) is one tap with reduced caps |
| Automatic updates | Server pushes new code per request | Content-addressed signed updates via Opifex, atomic, no per-launch download (§14) |
| Works from any device (kiosks, friends' machines) | No install needed | Remote rendering via Aqueduct + Fresco (§20.2); trial-launch for ephemeral use |

All four of the web's "killer features" in this category
have direct Insula equivalents — and Insula's are better on
every axis except one genuine edge case (§0.6.4).

### 0.6.2 The hidden engineering tax

The cost of maintaining web versions of desktop apps is
**enormous and largely invisible**. Industry examples:

- Microsoft maintains Excel for Windows, Excel for Mac,
  Excel for iOS, Excel for Android, Excel Online, and
  Excel for the web in Teams — six codebases with subtly
  different feature coverage. The web version is famously
  the most limited and the most expensive per-feature to
  extend, because every interaction must be re-implemented
  in browser idioms.
- Figma ships only a web version, but it took years and
  ~30 MB of WebAssembly to make it remotely fast. A native
  Figma on Insula is a fraction of that size and faster.
- Slack / Discord / Notion are Electron — desktop apps
  that ship a browser to run their own web app. Each user
  has effectively installed a copy of Chromium per
  installed Electron app.

Half the engineering at modern productivity companies is
duplicating desktop functionality on the web. Eliminate
that duplication and the same teams ship *features* instead
of *parity*. This is a multi-billion-dollar industry-wide
opportunity cost that vanishes here.

### 0.6.3 The user-facing flow that replaces "web version"

A user encountering an `.xlsx` attachment on a non-Atrium
world today: open browser → navigate to Excel Online → sign
in → upload → edit → download → attach back. Many minutes,
poor fidelity, server round-trip per keystroke for
collaboration features.

On Insula:

- **App installed locally:** attachment opens in the local
  app. Saving writes back wherever the user's data lives.
- **App not installed:** the attachment handler offers (a)
  install with capability-consent review, or (b) trial-
  launch (§0.5.3) with reduced caps for a one-time view/
  edit session.
- **User on a friend's Atrium machine:** trial-launch lets
  them edit; saving writes to their cloud-backed storage,
  bound to their Vestibulum identity (§13). No "log in to
  Excel Online" ceremony.
- **User on a non-Atrium device:** remote rendering
  (§20.2) — the user's home Atrium machine or a rented
  Atrium cloud instance runs the app and renders to a thin
  Atrium-rendering client on the non-Atrium device.

All of this is *better* than today's web version, and none
of it requires the vendor to maintain a separate web
codebase.

### 0.6.4 Vendor-hosted web access — the loop closes

The cleanest answer for "I need to access my app from a
non-Atrium machine I do not own" is **not** a separate web
version of the app. It is a **website that serves a remote-
rendered native session**.

Mechanism:

- The vendor compiles their app once for Atrium.
- The vendor hosts Atrium servers running the real app.
- The vendor's website (`excel.microsoft.com`, etc.) serves
  a thin Atrium scenegraph renderer that runs in a browser
  — a WASM/JS implementation of the Fresco rendering side
  plus an input forwarder.
- Visiting the URL: browser-side renderer connects to a
  remote Insula session running the real native app on the
  vendor's infrastructure.

What the user sees: "Excel in a browser tab." What
Microsoft maintains: one native Excel codebase plus a
hosting fleet — no second app to write.

The web becomes a *transport for the Atrium experience*,
not a *platform that competes with Atrium*.

This is the Citrix Receiver / VMware Horizon model, but
open, vendor-neutral, default for the platform, and
integrated with Vestibulum identity for transparent sign-in.

### 0.6.5 The "one codebase serves everywhere" pattern

A single vendor binary reaches the entire user universe via
three deployment topologies, all sharing the same code path
underneath:

| Topology | Renderer | Compute | Used for |
|---|---|---|---|
| Local | Local Fresco compositor | Local device | Daily use on the user's Atrium devices |
| Remote → Atrium device | Remote Atrium device's compositor | Remote Atrium machine | User on their phone connecting to their desktop; thin client; collaborative session |
| Remote → non-Atrium device | Browser-side scenegraph renderer (the web viewer) | Vendor server or user's home server | Library kiosk; friend's Chromebook; hotel business center |

ONE codebase. The web becomes one of the renderers, not a
parallel platform.

Who hosts the remote-render backend is independent of who
builds the app — three clean options:

- **Vendor-hosted.** Microsoft hosts Atrium-Excel servers
  behind `excel.microsoft.com`. Vendor pays compute,
  controls update cadence. Closest to today's Office
  Online.
- **User-hosted.** User's home Atrium machine runs the
  real app; the vendor's website (or any compatible web
  viewer) is just a bootstrap that connects to the user's
  own server. User pays effectively nothing; data never
  leaves their network.
- **Third-party cloud.** A separate provider rents
  Atrium-server slices as a service ("your apps,
  accessible from anywhere"). User picks their host; the
  app's vendor is not involved.

The user's *data location* is orthogonal to the *compute
location*: data lives wherever the user keeps it (local
Tessera, their cloud storage, the vendor's storage). The
web conflates these; Insula does not.

### 0.6.6 Why this matters beyond the design

This is the strongest *pragmatic* (not aesthetic, not
safety) argument for Insula:

- It is the argument that resonates with industry readers
  who would otherwise dismiss this as "another desktop
  platform fighting the web."
- It maps to a real economic problem (multi-billion-dollar
  duplication cost) that vendors privately acknowledge but
  publicly cannot escape because the web is the floor.
- It implies a different go-to-market story than "convince
  users to switch": *vendors* benefit first by eliminating
  their web-version maintenance burden, and users benefit
  downstream from getting the better (desktop-class)
  experience.

## 0.7 Portability and bring-up strategy

Insula's contract is OS-agnostic by construction —
Aqueduct, Pergola, the manifest format, the userspace
services (Limen, Tabellarius, Loculus, Concursus,
Nomenclator) are portable. Only a per-OS **host adapter**
is platform-specific. Insula is meant to ship on macOS,
Linux, and Windows alongside Atrium, not as an
Atrium-exclusive layer.

### 0.7.1 macOS-first bring-up

Four reasons stacked:

1. **Adoption pre-Atrium.** macOS has developers and users;
   Atrium has neither yet. Insula on macOS lets the model
   accumulate evidence — real apps, real users, real
   friction — before betting on the OS. De-risks the
   platform claim by proving it works without the
   platform.
2. **Forces clean portability.** Building FreeBSD-first
   bakes in FreeBSD-isms invisibly (jail, devfs, rctl,
   Capsicum). macOS-first surfaces the abstractions from
   day one. macOS is a *good* portability test: Unix-
   shaped enough to feel familiar, different-enough
   sandbox / IPC / process primitives to keep contracts
   honest.
3. **Validates "Aqueduct/Pergola are portable substrates."**
   The claim is theoretical until macOS makes it concrete.
   A working Insula-on-macOS *is* the evidence.
4. **Lowers activation energy by orders of magnitude.**
   `brew install insula-sdk` and a developer is building
   their first app in 10 minutes. The alternative ("first
   install this experimental OS in a VM") is a friction
   wall most developers will not cross. Once apps exist
   on macOS, porting to Atrium becomes "another supported
   target," not "a leap of faith."

### 0.7.2 The host-adapter abstraction

Insula's contract is OS-agnostic. The bottom of the stack
is host-specific. The boundary:

| Insula concept | Atrium implementation | macOS implementation | Linux implementation | Windows implementation |
|---|---|---|---|---|
| Sandbox boundary | Portcullis (FreeBSD jail) | App Sandbox / Sandbox.kext + entitlements | Landlock + seccomp + namespaces (bubblewrap-shape) | AppContainer + Job objects |
| Service launch | jaild + portcullisd | launchd + sandbox profiles | systemd + cgroups, or standalone supervisor | Service Control Manager / WinRT |
| Capability enforcement | jail manifest + Capsicum allowlist | sandbox profile + entitlements | seccomp filter + Landlock ruleset | AppContainer capabilities + WinRT brokers |
| Per-app networking | vnet jail + atrium-netd | Network Extension / Network.framework + broker | netns + nftables + broker | WFP + broker |
| Identity / keychain | Vestibulum (native) | macOS Keychain wrapped behind Vestibulum API | Secret Service / libsecret wrapped | Credential Manager wrapped |
| Resource limits | rctl | sandbox limits + posix `setrlimit` | cgroups v2 | Job Object limits |

**Userspace services (Limen, Tabellarius, Loculus,
Concursus, Nomenclator) are the same code on every host.**
The host adapter is the only per-OS piece.

This means the engineering effort to port Insula to a new
OS is bounded — write one host adapter (~5–10K LoC, mostly
sandbox + service launch + a thin networking shim) and
everything else compiles and runs.

### 0.7.3 Bring-up phases

| Phase | Target | Goal |
|---|---|---|
| 1 | **macOS** | Reference SDK, reference IDE (§10.9), sample apps, Limen + Tabellarius + Loculus + Concursus + Nomenclator running on the macOS host adapter. Prove the contract end-to-end. |
| 2 | **Linux** | Second host adapter (Landlock + seccomp + namespaces). Proves the contract is really portable, not "Apple-Unix-only." Adds the second-largest desktop user base. |
| 3 | **Windows** | Third host adapter (AppContainer + Job objects). Three-OS coverage = developers stop worrying about platform availability. |
| 4 | **Atrium** | Purpose-built Insula host. All the existing FreeBSD work culminates here. Atrium becomes "the OS that does Insula natively, with kernel-enforced jails + native GPU + native CAS-FS dedup." Best Insula experience, not the only one. |

### 0.7.4 What is already in place

Significant macOS-host work already exists in the broader
Atrium codebase as part of the D0–D1 bring-up:

- **Fresco runs on macOS** via venus → MoltenVK → Metal
  (validated 2026-05-10, Cocoa window painted from Atrium
  scenegraph).
- **Aqueduct charter** explicitly mentions OS-agnostic
  portability across BSDs / Linux / non-POSIX.
- **macOS-host cross-compile workflow** is already the
  daily-iteration shape (rust-lld + sysroot CRT in
  `~/src/bsd/.cargo/config.toml`).
- **kqueue is the event multiplexer** by design — and
  macOS has kqueue natively.

Insula on macOS reuses this foundation; only the new
Insula-specific services (Limen, Tabellarius, etc.) and
the macOS host adapter are net-new work.

### 0.7.5 Atrium's repositioned pitch

The phasing changes how Atrium is sold:

- **Old framing:** "Switch to this new OS to run these new apps."
- **New framing:** "Insula apps run on your Mac/Linux/Windows
  today. Atrium is the OS purpose-built for Insula — best
  performance, best security, kernel-enforced sandbox,
  content-addressed FS — for users who want the maximum
  experience."

The OS becomes a *destination*, not a *prerequisite*. Users
encounter Insula apps long before they encounter Atrium.
Atrium then has a real value proposition for the users
who graduate to it (security, perf, native primitives)
rather than competing for cold-start adoption.

### 0.7.6 What this means for the spec body

The rest of this spec uses **Atrium-canonical terminology**
(Portcullis, FreeBSD jail, Capsicum, rctl, vnet) because
Atrium is the reference implementation. Where the spec
says "Portcullis," read "Portcullis on Atrium; the host-
adapter equivalent on macOS / Linux / Windows." The
*Insula contract* is the same regardless of host; the
*implementation primitive* varies per the table in §0.7.2.

```
publisher                target device
─────────                ─────────────
edit source              receive bundle
  │                        │
  ▼                        ▼
compile (LLVM)           verify signature
  │                        │
  ▼                        ▼
sign bundle              [AOT compile if IR]
  │                        │
  ▼                        ▼
Tessera CAS              register manifest
  │                        │
  └────── publish ─────────┤
                           ▼
                         launch:
                           Portcullis builds jail
                           jail execs native ELF
                           ELF dlopens libatrium.so
                           opens fresco / aqueduct
                           ...
```

The runtime form is **always** a native ELF in a jail linking
against `libatrium.so`. The distribution form may be native or
IR; both converge at the runtime.

## 2. ABI

### 2.1 Calling convention

Standard System V PCS for the target architecture. No new ABI.
Stock `clang`, `rustc`, `zig cc`, etc. emit binaries that
target Atrium with no modification beyond linking against the
platform headers and library.

### 2.2 Syscall surface

The jail restricts available syscalls to a Capsicum-shaped
allowlist. Roughly:

| Allowed directly | Proxied via Aqueduct | Forbidden |
|---|---|---|
| `read`, `write` on existing fds | `open`, `connect`, `bind` | `mount`, `kld*`, `jail_*` |
| `mmap`, `munmap`, `mprotect` | DNS resolution | `ptrace` |
| `kevent`, `kqueue` | filesystem traversal beyond namespace | raw sockets (unless capability) |
| `clock_gettime`, `nanosleep` | network connections | most `sysctl` |
| futex-equivalent, condvar primitives | timers, notifications | `setuid` and friends |
| `_exit`, signal primitives | clipboard, picker, share | `chroot`, `jail` |

Apps that call a forbidden syscall get `ECAPMODE` (Capsicum-
style), not a hidden translation. The allowlist is versioned
and frozen as part of the platform ABI; new entries follow the
same semver discipline as the platform library.

### 2.3 Platform library (`libatrium.so`)

A C-ABI shared object provided by the system. Wraps the
Aqueduct services that scripts can reach. Sketch of the
surface:

- **`atrium_fresco_*`** — open a window, present a frame,
  receive input events. Pergola sits on top of this.
- **`atrium_storage_*`** — read/write within the app's
  Tessera namespace.
- **`atrium_net_*`** — connect to a hostname:port allowed by
  the manifest. Resolves via the network broker (§4).
- **`atrium_picker_*`** — invoke a system file/share/...
  picker to obtain a one-shot capability (§5.2).
- **`atrium_clipboard_*`**, **`atrium_notify_*`**,
  **`atrium_timer_*`**, ...

Public SDKs (Rust, C) are thin wrappers over `libatrium.so`.
Third-party SDKs (Zig, Swift, Go, ...) are community.

### 2.4 Versioning

`libatrium.so` versions follow `MAJOR.MINOR`. Manifest declares
the minimum `sdk-version` the app needs. The launcher refuses
to start an app whose declared version exceeds the installed
platform's version. Backward-compatible additions only within
a MAJOR.

## 3. Distribution

### 3.1 Bundle shape

A signed bundle in Tessera, content-addressed by its root
hash:

```
my-app/
  manifest.toml         # see §5
  signature             # publisher signature over manifest+contents
  bin/
    my-app              # native ELF, or IR artifact (§3.3)
  assets/
    ...
```

Install = verify signature, resolve through Tessera CAS,
register manifest with Portcullis. The same bundle is byte-
identical on every device that has it.

### 3.2 Native distribution (default)

Publishers compile to native ELF for each supported Atrium
architecture and ship the binaries in the bundle. Trivially
the best path:

- Smallest install action: verify + register only.
- Best runtime quality: publisher controls optimization level.
- Smallest cold start: no AOT step.

Cost: one ELF per architecture (or one fat binary).

### 3.3 IR distribution (optional)

For cross-arch portability, publishers may ship a portable IR
artifact instead. The realistic format is **WASM** — not as a
sandbox, but as a stable, well-toolchained, language-agnostic
portable assembly:

- Stable bytecode format across versions.
- Clang/Rustc/Zig/TinyGo/etc. all emit it.
- Cranelift natively consumes it (wasmtime backend).
- The browser's WASI / sandboxing model is *not* used; the
  jail is the sandbox. WASM modules link (at AOT time)
  against `libatrium.so` via a thin import-shim. Output is a
  normal native ELF.

Cranelift produces ~70% of LLVM's runtime quality at ~5–10×
faster compile. That is the right tradeoff for install-time
AOT: the resulting native artifact is then mmap'd on every
launch.

The bespoke SPIR-V backend (atrium-spv-backend-bespoke) is
**not** used in this path. It is a specialized shader backend
and inappropriate for general-purpose code.

### 3.4 Install caching

The AOT result for a given (IR-bundle-hash, target-arch,
sdk-version) tuple is cached in Tessera, keyed by the tuple.
Subsequent installs of the same bundle skip the AOT step.

## 4. Sandbox and network capability

> *Per §0.7.6, this section describes the Atrium-canonical
> implementation. On macOS / Linux / Windows, the same
> guarantees are provided by the host adapter — App
> Sandbox, Landlock+seccomp, AppContainer respectively —
> with equivalent capability shape but per-OS primitives.
> See §0.7.2 for the mapping table.*

### 4.1 Jail shape

Each app instance runs in its own Portcullis jail with:

- A private Tessera namespace mounted at a known path.
- A devfs-restricted view (no raw `/dev/*` access except what
  the manifest declares).
- A vnet jail with no default route.
- Access to Aqueduct sockets allowed by the manifest.
- `rctl` limits on CPU / RSS / wall time / fds.

The jail is the security boundary, *not* the platform library.
A compromised `libatrium.so` does not let an app escape; only
the kernel can.

### 4.2 Network model

The web's CORS / CSP / SOP / fetch story is replaced by a
**network capability broker**:

- The vnet jail has no direct network access.
- `atrium_net_connect("api.example.com", 443)` goes to the
  broker over a unix socket.
- The broker checks the manifest (§5.1), resolves DNS itself,
  opens the underlying connection, and hands the resulting
  fd back to the jail.
- Per-request policy (allowed methods, TLS pinning, allowed
  paths) lives in the manifest, enforced by the broker.

For tools genuinely needing raw network access, a
`raw-network` capability bypasses the broker but is loudly
disclosed in the install-time consent UI.

Cost: ~500 µs per `connect`. Invisible against TCP+TLS.

## 5. Manifest and capabilities

### 5.1 Static capabilities (declared)

Capabilities declared at install time, enforced by Portcullis:

```toml
[app]
name = "example.com.weather"
version = "1.2.3"
sdk-version = "1.x"

[bundle]
form = "native"                # or "wasm"
arches = ["aarch64-freebsd"]
entry = "bin/weather"

[render]
fresco = true                  # opens its own windows

[input]
keyboard = "focus"             # only when window is focused
pointer = "focus"

[network]
hosts = [
  { name = "api.weather.example.com", port = 443, proto = "tcp" },
]

[storage]
namespace = "example.com.weather"
quota = "100MB"

[ipc]
services = ["fresco-protocol", "clipboard"]

[compute]
cpu = "100ms/s"
rss = "256MB"
wall = "unbounded"
```

User sees the capability list **once**, at install time. No
ambient runtime permission prompts.

### 5.2 Dynamic capabilities (powerbox)

Capabilities the app cannot declare upfront are minted at
use-time by *system-trusted UI* (Scrinium file picker,
share sheet,
device picker) running outside the jail. The picker hands the
jail a fresh fd / capability for the specific resource the
user pointed at — never ambient access to all files.

This is the KeyKOS / Capsicum-lineage "powerbox" pattern.

Sketches the right answer to the "allow access to all your
photos forever?" disaster: that prompt never happens because
the app cannot ask. The user *points* at a photo and the
system *gives* the app that one photo.

### 5.3 Capability shape

| Capability | Lifetime | Mint by |
|---|---|---|
| Network host | install | manifest |
| Persistent storage | install | manifest |
| IPC service | install | manifest |
| Specific file fd | one session | picker |
| Share target fd | one operation | share sheet |
| Device fd | one session | device picker |
| Clipboard read | one paste | clipboard service |

## 6. Source language

Polyglot at the ABI; the platform owns the contract, not the
language.

### 6.1 First-party SDKs

- **Rust** — idiomatic wrapper crate over `libatrium.so`,
  shipped with the Atrium SDK. Primary language.
- **C** — headers + `libatrium.so`, shipped with the SDK.

### 6.2 Community languages

Any language that emits ELF (Zig, Swift, Go, Crystal, Nim, …)
or has a WASM target (TinyGo, AssemblyScript, …) can target
Atrium. Bindings are not blessed by the platform.

### 6.3 Scripting languages

Python, Lua, JavaScript-as-a-language, etc. are *user-space
choices*, not platform-blessed runtimes. A Python interpreter
is a regular Atrium app (jailed, native-compiled CPython);
`.py` files are its inputs. The capability boundary is around
the interpreter; users accepting "the interpreter enforces my
script's intent" is a separate, weaker trust statement.

The platform does not ship a language. Browsers shipped JS
because they had to; Atrium does not have to.

## 7. Codegen split

| Backend | Used for | Notes |
|---|---|---|
| LLVM | publisher-side native build | Production -O3. Runs on dev machines, not target. |
| Cranelift | install-time AOT of IR bundles | ~70% LLVM quality at ~5–10× faster compile. |
| `atrium-spv-backend-bespoke` | shaders (tier-2 Vulkan) | Not used in app scripting. Different IR, different shape. |
| Interpreters | user-chosen scripting languages | Shipped as regular apps. |

The bespoke shader codegen has **no role** in app execution.
The temptation to reuse it for scripting is misleading — its
IR (atrium-spv-ir) is shader-shaped and inappropriate for
general-purpose code.

## 8. Cold-start budget

Component costs (rough, aarch64 FreeBSD):

| Step | Cost |
|---|---|
| fork | ~50 µs |
| exec + dynamic link | ~1–2 ms |
| jail attach + cred setup | ~100–500 µs |
| broker handshake | ~500 µs |
| First Fresco connect | ~200 µs |
| First frame paint | ~16 ms (frame-cadence-bound) |
| **Total cold launch → first pixel** | **~20 ms** |

Already inside "feels instant." A small pool of ~8 pre-jailed
empty processes (~8 MB resident total) cuts the launch path
to ~200 µs for the case where hover-preview UX needs it. No
process resurrection / CRIU equivalent required.

Persistence of *compiled state* is automatic via Tessera CAS:
once AOT-compiled, the native artifact is mmap'd on every
launch. The browser's tiered JIT warmup has no analogue
because there is no JIT.

## 9. Dev iteration

The honest tradeoff vs. browser F5: with native compile, the
inner loop is bounded by compile time, not platform overhead.

### 9.1 Where time goes (Rust, incremental)

| Step | Cost |
|---|---|
| Edit save | 0 |
| `cargo build` incremental | ~2 s |
| Re-sign + repackage (or skipped in dev) | ~10 ms / 0 |
| Re-install (Tessera CAS + manifest update) | ~50 ms |
| Re-launch (fork+exec+jail from pool) | ~200 µs |
| First frame after relaunch | ~16 ms |
| **Total** | **~2.1 s** |

Compile dominates. **Compile time is a language-toolchain
property, not a platform property.**

### 9.2 `portcullis dev` mode

A first-class dev workflow:

```
$ portcullis dev ./my-app/
[watch]   source tree
[build]   incremental rebuild on save
[install] Tessera CAS + manifest update
[launch]  killing previous instance, spawning in jail 'dev-…'
[ready]   window opened, stderr streaming to terminal
```

Dev mode:

- Skips publisher-grade signing (uses a dev signing key the
  system trusts only when launched via `portcullis dev`).
- Includes debug symbols.
- Opens broader capabilities for tooling: lldb attach,
  dtrace USDT probes pass through, broker logs requests.

### 9.3 State preservation across relaunch

The platform offers a `state` capability — a stable KV in the
app's Tessera namespace, flushed on SIGTERM, restored on
launch. Apps written with explicit suspend/resume semantics
get HMR-feel for free; the dev relaunch *feels* like a hot-
reload even though it is a real cold start.

Same design works for production (crash recovery, OS updates,
suspend/resume). Two birds.

### 9.4 Faster dev backends

For developers who want F5-grade iteration on Rust, the
`rustc_codegen_cranelift` backend (already known in the
ecosystem) shaves a real chunk off `cargo build` for dev
profile. Production builds still use LLVM.

### 9.5 Interpreted-language option

For projects where the F5 loop matters more than runtime perf,
the right answer is "use an interpreted language for that
project." Python / Lua / Tcl scripts on Atrium reload in
milliseconds because the interpreter is a long-lived jailed
process; reloading is sending it a new source buffer.

### 9.6 DevTools / introspection

The Atrium equivalent of browser DevTools exists for free as
soon as one builds the UI — every observation surface is
already present:

- Fresco knows the full scene tree.
- Aqueduct knows every IPC message.
- The network broker knows every request.
- The kernel exposes syscall traffic via `truss` / dtrace.
- The bespoke / Cranelift backends emit a PC-map sidecar so
  native PCs map back to source.

A "DevTools" Atrium app subscribes to these and presents
scene tree, IPC log, network log, capability access log, perf
counters, stack samples. Engine = dtrace; UI = an app.
Production builds opt out of inspection; dev builds always
allow.

## 10. UI model — no DOM

The DOM conflates two things: a structured text-document
data model and a shared retained-mode UI tree mutated by code.
Native binaries separate them cleanly.

### 10.1 Apps

Every "website" that is *actually an application* (Gmail,
Figma, Notion, Slack, GitHub UI, ...) becomes a native Atrium
app. Renders via Pergola windows over Fresco. No HTML, no CSS,
no DOM. The toolkit ships with the platform; the app does not
redownload React-shaped runtime per visit.

### 10.2 Documents

Documents (Wikipedia, blogs, news articles, papers) are
content, not programs. A **document viewer app** parses some
text-document format (HTML+CSS, Markdown, or a cleaner
format) and renders via Pergola. The viewer is one app among
many; users can swap it.

### 10.3 Cross-app composition

The browser got the *idea* of `<iframe>` right (embed
renderable content from a different trust domain) and the
*implementation* wrong (in-process sandbox, SOP-as-trust-
boundary, untyped `postMessage`). Atrium keeps the idea and
discards the constraints.

**The shape:** a parent app's window contains a rectangular
slot rendered by a child app in a separate jail. The
compositor stitches the result; input within the slot routes
to the child; communication is a typed message channel.

#### 10.3.1 Launch model — Limen

The parent does **not** launch the child directly. A trusted
system service, **Limen** (Latin: *threshold* — the boundary
across which app surfaces meet), mediates:

1. Parent: `request_embed(role)`.
2. Limen looks up the user's preferred app for that role
   (or platform default).
3. Limen asks Portcullis to launch the selected app in
   embed mode, with capabilities from *its own* manifest.
4. Limen wires up a Fresco slot and a typed message
   channel between parent and child.
5. Both ends get `attached`.

**Why broker-mediated.** Parent declares *intent* ("I need
a doc-viewer"), not *implementation* ("launch this binary").
The child's capabilities come from its own manifest, not
the parent's. Neither side has authority over the other.
This is the powerbox pattern (§5.2) applied to rendering.

#### 10.3.2 Embed roles — typed contracts

Each embed has a declared role (string identifier). A role
defines a typed protocol both sides speak. Initial set:

| Role | Parent → Child | Child → Parent |
|---|---|---|
| `doc-viewer` | `load(url)`, `set_theme(...)` | `loaded`, `error(...)`, `link_clicked(url)`, `selection(...)` |
| `media-player` | `play`, `pause`, `seek(t)` | `time_update(t)`, `ended`, `error` |
| `picker` | `open(filter)` | `picked(fd)`, `cancelled` |
| `payment` | `start(amount, currency, ref)` | `completed(receipt)`, `cancelled` |
| `map` | `set_view(...)`, `add_marker(...)` | `marker_clicked(id)`, `zoom_changed(...)` |
| `share-target` | `share(content)` | `accepted`, `declined` |

Roles are platform-defined, versioned, frozen with the
platform ABI. Apps declare which roles they implement
(child side) or request (parent side) in their manifest.

The system enforces well-typed messages on the channel — a
parent cannot send arbitrary bytes to a child, only
messages from the role's protocol. Eliminates the
postMessage-as-untyped-RPC bug class.

#### 10.3.3 Rendering — Fresco surface slots

- Parent creates a slot: `(rect, role, options)`.
- Compositor reserves the region; broker hands the slot ID
  to the child.
- Child renders into the slot like any other Fresco
  surface; full Pergola access on its side.
- Composition: parent and child surfaces stitched in
  z-order. Parent can layer overlays *above* the slot but
  **never mutate or read** the slot's contents.

**Pixel readback is disallowed.** The compositor never
gives the parent access to the child's rendered bytes.
This blocks the attack class where a malicious parent
embeds a trusted child to harvest displayed content
(banking, addresses, …). Strictly stronger than the web's
`<canvas>` taint rules, which are partial and retrofitted.

#### 10.3.4 Input routing

- Pointer events within slot → child. Outside → parent.
- Keyboard events when child has focus → child. Otherwise →
  parent's focused widget. System chords → WM.
- Scroll within child → child.
- Drag-and-drop across the boundary → mediated by system
  DnD service (powerbox: one-shot capability grant from
  user gesture).

Parent can request `EMBED_INPUT_NONE` for decorative slots;
child can declare itself read-only.

#### 10.3.5 Lifecycle

State machine observable from the parent:

- `attached` — child rendering, channel open.
- `child_lost` — crash or kill. Compositor shows last frame
  or placeholder. Parent decides whether to ask broker to
  relaunch.
- `detached` — orderly shutdown.

**A child crash cannot affect the parent.** Resource
accounting per-jail (`rctl`); the child's consumption
counts against its own quota.

#### 10.3.6 Side channels

| Channel | Mitigation |
|---|---|
| Pixel readback | Compositor never gives parent the bytes. |
| Render-completion timing | Only role-level events surface to parent; compositor coalesces. |
| Shared GPU caches (Spectre-class) | Per-jail GPU contexts. Tier-1 GPU isolation is a separate spec; this design piggybacks. |
| Audio capture | Separate capability, default-deny. Embedded children do not get parent's audio context. |
| Storage / network sharing | Each jail has its own; embedding does not connect them. |

#### 10.3.7 Accessibility

The AX tree spans the boundary (§10.4 decision 4). The
`atrium-ax` service stitches the child's AX subtree into
the parent's tree at the slot position. Screen readers see
one coherent tree. The slot is a tree node with
`role=embedded-content`; descendants come from the child.

Strictly better than browser cross-origin iframes, where
SOP blocks AX traversal entirely.

#### 10.3.8 API sketch

Parent:

```c
atrium_embed_t slot = atrium_embed_request(
    window, rect, "doc-viewer",
    .input = EMBED_INPUT_FULL,
    .audio = EMBED_AUDIO_NONE,
    .transparency = EMBED_TRANSPARENCY_OPAQUE
);
atrium_embed_send(slot, "load", "atrium-doc://abc123");

while (atrium_embed_poll(slot, &ev)) {
    switch (ev.kind) {
    case EMBED_ATTACHED: break;
    case EMBED_MESSAGE:
        if (streq(ev.msg.name, "link_clicked"))
            handle_link(ev.msg.payload);
        break;
    case EMBED_LOST:
        placeholder(); break;
    }
}
```

Child (launched in embed mode by broker):

```c
atrium_embed_self_t self = atrium_embed_self_attach();

while (atrium_embed_self_poll(self, &msg)) {
    if (streq(msg.name, "load"))
        load_and_render(msg.payload);
}

atrium_embed_self_emit(self, "link_clicked", clicked_url);
```

Both sides see only role-typed messages. Neither sees the
other's process, surface, or capabilities.

### 10.4 Accessibility (the genuine regression risk)

Browsers give screen readers a structured tree from the DOM
for free. Native toolkits historically botch a11y because
each rolls its own. The web's strength was *not duplication*:
the DOM **is** the a11y tree, so apps cannot accidentally
bypass it. Atrium has the same opportunity if Pergola is
designed correctly from the start, and the matching
vulnerability if it is not.

**Locked decisions for this design:**

1. **The AX tree IS the widget tree, not a sidecar.** Every
   Pergola widget has a role (`button`, `heading`,
   `text-input`, `region`, `landmark`, …), accessible name,
   state (focused, disabled, selected, expanded, …), and
   parent-child structure. Apps do not populate a separate
   AX API — using a Pergola widget *is* declaring its AX
   semantics.

2. **Custom-drawn UIs must declare a shadow AX tree.**
   Apps drawing their own UI (canvas-equivalent, custom
   visualization widgets) must publish an AX shadow tree
   describing what they drew. The inspector app surfaces
   "regions with no AX coverage" so this is visible at
   build time, not silent at runtime.

3. **`atrium-ax` is a first-class platform Aqueduct
   service**, not optional, not an add-on. The capability is
   granted to assistive-tech apps: screen readers, voice
   control, switch control, magnifiers. The service
   publishes:
   - Tree snapshot on request.
   - Incremental updates as widgets mutate (subscribe model
     with politeness levels for live regions).
   - Focus changes.
   - Activation requests inbound ("assistive tech asks to
     click button X").

   This is the AT-SPI / UIA shape, done as a first-party
   Aqueduct service from day one instead of bolted onto an
   existing toolkit.

4. **AX composes across jail boundaries.** Cross-app
   composition (§10.3) means a single visual UI may span
   multiple jailed processes. The AX service stitches the
   tree across jails — assistive tech sees one coherent
   tree. Capability gate is the same as Fresco composition.
   Strictly better than browser cross-origin iframes, where
   SOP blocks screen reader traversal and a11y degrades
   silently.

5. **AX coverage is a publish-time gate.** App
   signing / certification refuses bundles whose AX coverage
   falls below a threshold. The same introspection surface
   the inspector app uses computes the metric. A11y is a
   build-time concern, not a "we'll fix it later" concern.

6. **Document viewer's AX bridge is platform-class.** The
   document viewer (§10.2) is the place text-document
   semantics (headings, lists, links, tables, captions)
   must survive into the AX tree. One reference viewer with
   strong AX is platform-blessed; community viewers are
   permitted but warned to users when their AX coverage is
   weaker.

**Genuinely hard sub-problems (open):**

- **Live regions / politeness levels.** "New message
  arrived" should be announced; "cursor moved" should not.
  Pergola needs explicit primitives for live regions and
  the politeness scale, matching ARIA-live semantics but
  without ARIA's syntactic awkwardness.
- **Spatial vs. structural order.** Visual reading order may
  not match widget-tree order (sidebars, floating panels).
  The AX tree must expose both, and the assistive tech
  picks.
- **i18n of accessible names.** AX names follow the same
  locale path as visible text. The AX layer must be locale-
  aware, not English-as-default.
- **Subscription throttling.** A widget that mutates every
  frame must not produce an event per frame on the AX
  stream. Coalescing rules need design; web browsers
  learned this the hard way.

These are queued for the Pergola spec proper, not resolved
here.

### 10.5 Inspector

The "view source / inspect element" property is recovered by
an inspector app that connects to Pergola's introspection
API. Production apps can opt out of inspection in release
builds; dev builds always allow.

### 10.6 The document viewer

The document-shaped subset of the web (Wikipedia, blogs,
news, papers, READMEs, PDFs, docs sites) is served by a
platform-class **document viewer app**. One Atrium app among
many, with no special runtime privileges, but blessed as the
reference renderer.

**Why it deserves dedicated design:**

- Documents are declarative content, not code. The jail
  rendering a document needs *zero* network/storage/IPC
  capabilities beyond rendering. The viewer is the trust
  boundary; documents are inert.
- Users read many documents per day. Friction has to be
  near-zero — this is what tempts platforms to keep a
  browser. We keep the experience without the architecture.

**Format strategy:** ship multiple, parsers are pluggable
modules in one viewer, all emit the same internal Pergola-
shaped IR with AX semantics baked in.

- **Canonical authoring format:** a Markdown superset with
  explicit AX-aware extensions (figure/figcaption, semantic
  block roles, named regions, table headers). This is what
  people write.
- **HTML+CSS subset:** for legacy web content. A sanitizer
  strips JS, normalizes CSS to a bounded layout-primitive
  set, feeds the same IR. 95% of legacy documents work;
  broken 5% degrade visibly (no silent broken renders).
- **PDF:** another backend in the same viewer, or a sibling
  viewer app.

**Network model:**

- The viewer has a broad `[network]` capability (it's a
  general-purpose reader).
- Fetched bytes are loaded into a **fresh inner sub-jail**
  with zero network. The viewer mediates; the document is
  inert.
- Per-document state (scroll position, bookmarks) is the
  *viewer's* storage. Documents have no persistent state.

This is strictly cleaner than browser SOP. The browser's
origin-as-trust-boundary forced same-origin policy because
pages execute. Inert documents don't need SOP.

**Addressing — content-addressed by default:**

- Document URLs are content addresses: `atrium-doc://<hash>`.
- A layer of human-readable indirection (DNS-equivalent +
  signed publisher manifest) resolves "the current version
  of this article" to a hash, then content-addresses the
  document.
- Documents are **cacheable forever** because the hash is
  the address. Offline reading falls out for free.
- Link rot is a *publisher* problem (rotating the
  human-readable indirection), not an archival catastrophe.

This is the part of the web that should have always been
content-addressed.

**Deep links:** `atrium-doc://<hash>#section-id`. Document
structural anchors (heading IDs, named regions) are
addressable; the viewer scrolls and announces ("jumped to
section: Introduction") to assistive tech.

**Embedding inside apps:** apps frequently need to render
documents (README in a code editor, article in a news app,
help text anywhere). Pattern is cross-jail Fresco
composition (§10.3): the parent app embeds the viewer's
rendered surface as a child. Parent never sees the
document's raw bytes, only the rendered surface. This is
what `<iframe srcdoc>` should have been.

**Why the canonical authoring format isn't HTML:** HTML+CSS
is technically a programming language at this point
(container queries, anchor positioning, CSS-in-JS, layout
algorithms that can't be statically analyzed). The "CSS
subset" line is endlessly contested. A clean format
designed-for-AX-from-the-start is the right thing to write
new content in; HTML+CSS support is the migration path, not
the destination.

### 10.7 Declarative UI

A React / SwiftUI / Compose-shaped declarative API is
available as a *library* on top of Pergola, not a platform
mandate. The choice is at the app level, not the platform
level — the inverse of the browser's situation.

### 10.8 Visual identity vs. interaction conventions

A predictable reaction from web-shaped readers: "if every
app is a Pergola app, they will all look the same — that
loses the brand-expression freedom the web is famous for."
The honest answer is that Pergola opinionates one layer and
leaves the other free, and the layer it opinionates is the
one the web abused.

**Pergola is opinionated about *interaction primitives*:**

| Property | Pergola guarantee |
|---|---|
| Input model | Standard — predictable across apps |
| Focus / keyboard navigation | Always works, AX-traversable |
| Accessibility tree | Structural property of the widget tree (§10.4) |
| System gestures (back, quit, switch, screenshot) | OS-mediated; apps cannot capture |
| Hit areas, target sizes, click semantics | Defaults; certification gates outliers |
| Scroll behavior, momentum, edge bounce | System-consistent; no "scroll-jacking" |
| Right-click / context menus | System-mediated, predictable shape |

The web's "freedom" in these areas was *mostly* abused —
popups, scroll-jacking, fake back buttons, hidden close
affordances, dark patterns. Insula apps recovering them is
a real win for users even if it constrains designers.

**Pergola is liberal about *visual presentation*:**

| Property | Free to customize? |
|---|---|
| Color palette, brand colors | Yes — fully |
| Typography (within a11y limits) | Yes — fully |
| Custom illustrations, iconography | Yes — fully |
| Layout, composition, whitespace | Yes — fully |
| Custom-drawn regions (creative tools, viz, games) | Yes, with shadow AX tree (§10.4 decision 2) |
| Animation, micro-interactions | Yes — Pergola is GPU-accelerated |
| Card / list / timeline / immersive layouts | Yes |
| Background, hero imagery, atmospheric design | Yes |

The shape is closer to SwiftUI / Compose than to legacy
Win32 / GTK: opinionated about interaction, liberal about
presentation. A weather app can be a beautiful immersive
thing with motion graphics and still be 100% Pergola. A
Figma-shaped creative tool wraps a custom-drawn canvas
widget with an AX shadow tree inside otherwise-Pergola
chrome.

**The honest tradeoffs:**

What designers lose:
- The "reinvent every interaction" freedom. A bespoke 200px
  custom-physics dropdown that fights muscle memory cannot
  exist.
- The "look completely unlike anything else" feel. Some
  family resemblance through standard primitives is
  unavoidable.

What designers gain:
- Stop spending 60% of design+engineering time
  reimplementing scrollbars, date pickers, modals,
  comboboxes. Spend it on content, brand voice, micro-
  interactions, the genuinely differentiating parts.
- "Feels native" — historically a moat for native apps
  over webapps — becomes the default.

**The category that loses most:** "interaction-art" sites
where the interaction itself is the design statement
(custom scroll experiences as branding, navigation as
puzzle). These are documents at heart — the rendering of
the design *is* the content — and arguably belong to the
document-viewer authoring format (§10.6), not the apps
layer.

### 10.9 Dev tools and the Electron alternative

VS Code is currently the dominant developer environment,
and it is built on Electron — desktop-class apps built with
web technology. The strategic question for Insula: how do
we deliver an IDE that matches VS Code's developer
experience without inheriting Electron's costs (web-tech-
as-desktop-substrate is the wrong direction)?

This section is partly a demonstration that Insula's
existing primitives — Pergola + Limen + Aqueduct + Stoa —
already compose into a better answer than Electron, and
partly a checklist of what the Insula reference IDE will
need to lock in adoption.

#### 10.9.1 What Electron got right (and we keep)

Electron's win was not technical; it was alignment of four
features:

| Electron strength | Insula equivalent |
|---|---|
| One UI codebase across OSes | Aqueduct + Insula contract are OS-agnostic by design |
| Modern UI fidelity (animation, GPU text, custom layout) | Pergola |
| Easy extensibility in a popular language | Polyglot ABI (§6) + Limen `editor-extension` role |
| Familiar mental model for contributors | "Extensions are just Insula apps" — generalizes beyond JS |

#### 10.9.2 What Electron got wrong (and we do not repeat)

| Cost | Cause |
|---|---|
| 200–500 MB RAM idle | Each Electron app ships a full Chromium |
| 2–5 s cold start | Chromium init + V8 warmup + JS parse |
| Janky scroll, weak keyboard handling, broken a11y | Web-tech is fundamentally not a desktop interaction model; Electron papers over but never fixes |
| "Feels like a website" | Because it is one, in a chrome-less window |
| Battery drain | Always-running V8 + GPU compositing for trivial UI |
| Per-app Chromium duplication | Three Electron apps = three Chromiums in RAM |

The damning observation: **VS Code's strength is not
Electron, it is everything Microsoft built on top of
Electron** — LSP, DAP, tree-sitter integration, the
extension API. Those are language-agnostic protocols and
contracts. They could run on anything. Microsoft chose
Electron because it was the path of least resistance, not
because it was the right substrate.

#### 10.9.3 The Insula IDE composition

The IDE is a *demonstration* of Insula's claims, not a
special case. Most pieces already exist:

| Component | Insula answer |
|---|---|
| Editor itself | Native Pergola app — `atrium-edit`'s production-grade descendant, or a sibling editor app |
| Language services | **LSP** — already external-process; fits Aqueduct directly as a typed message protocol |
| Debugger | **DAP** — same shape as LSP, fits Aqueduct directly |
| Terminal | **Stoa embedded via Limen** — already a foundation service |
| Syntax / structure | Tree-sitter — native Rust/C library |
| Search | ripgrep-shaped, native |
| VCS | git via Stoa or direct, integrated UI |
| Extensions | **Limen role `editor-extension`** — extensions are jailed Insula processes embedded as UI regions or background services |
| AI assist (Copilot-shape) | LSP extension or first-party assist service; runs as a separate jailed process |

LSP and DAP being external-process protocols was already
the right shape for Atrium. Microsoft accidentally invented
the protocol they would have wanted for Insula.

#### 10.9.4 The extension model

VS Code's extension API is *constrained* by Electron:
extensions run in a separate Node process because Electron
cannot trust them inside the renderer; they communicate via
IPC against a defined API. Insula generalizes this cleanly:

- An extension is **a normal Insula app** declaring the
  `editor-extension` Limen role.
- The editor reserves typed slots: sidebar panels, status-
  bar segments, command-palette entries, gutter
  decorations, hover providers, formatter handlers, code-
  action providers.
- Extensions declare capabilities in their manifest (read
  workspace files, run shell commands, access LSP, talk to
  the network) and are subject to install-time consent +
  capability-diff updates (§14.2).
- The editor host **cannot tamper** with extension UI; the
  extension **cannot tamper** with the host UI. Both go
  through Limen surface composition (§10.3).

Concrete improvements over VS Code:

- **Extensions in any language.** Rust for performance,
  Python for scripting, Zig for low-level tooling, Go,
  Crystal, whatever. Not "JavaScript or transpile to it."
- **Sandboxed by default**, capability-shaped. No "trust
  this extension to do anything" prompt — the manifest
  declares exactly what it can access.
- **Crash / slow / malicious extension cannot affect the
  editor.** Separate jail, separate process, `rctl`-bounded.
  VS Code's "an extension is making the editor slow" mode
  is impossible.
- **Extensions update independently** of the editor via
  Opifex.

#### 10.9.5 Performance targets

| Metric | VS Code (Electron) | Insula IDE target |
|---|---|---|
| Cold start | 2–5 s | <100 ms |
| Idle RAM | 200–500 MB | 20–50 MB |
| Open 100 MB log | chokes | instant (mmap) |
| Battery while idle | non-trivial | near-zero |
| Spin up an extension | 100s of ms | ~5 ms (from jail pool, §8) |

These are not aspirational; they are what native code on
modern hardware does when no JavaScript runtime is layered
between the user and the work.

#### 10.9.6 Strategic angle

The IDE is the single most effective adoption argument
because developers care about their tools more than almost
anything else. A reference IDE that is *demonstrably
better* than VS Code on every developer-perceivable axis
(startup, RAM, big-file responsiveness, battery,
extensibility safety) is the trojan horse that gets Insula
into developers' hands.

It also closes a loop with §0.6.4: VS Code's "web version"
(vscode.dev) is the canonical example of where this design
ends up — a thin web renderer connecting to a remote native
session. Microsoft already accidentally built it. They just
do not have the platform primitives to do it cleanly.

## 11. Background tasks

The web's background-execution story is three overlapping
APIs (Service Workers, Web Workers, Background Sync / Push)
plus opaque OS-enforced budgets. Atrium has one model
because background work is just a jailed process without a
window.

### 11.1 Lifecycle classes

| Class | Lives | Scheduled by | Web analogue |
|---|---|---|---|
| Foreground | While user has a window open | User interaction | Normal page JS |
| Resident background | Continuously, low priority | Always-running jail | Service worker (kind of) |
| Triggered | Briefly, on event | System scheduler | Background Sync, Push |

A single app can declare any subset. All three use the same
primitive (`exec` in a jail); only scheduling discipline
differs.

### 11.2 Manifest declaration

```toml
[background.resident]
entry = "bin/sync-daemon"
priority = "low"
max-rss = "32MB"

[background.triggered]
entry = "bin/handle-event"
events = ["push", "alarm", "network-resume",
          "tessera-changed:/inbox"]
max-runtime = "30s"
max-invocations-per-hour = 12
```

`max-invocations-per-hour` is the equivalent of browsers'
opaque "background sync may run sometime" rules — except it
is published and the user sees it at install.

### 11.3 Resident background

A long-lived jailed process surviving foreground close.
Examples: chat-app connection holder, sync daemon, music
player.

- Launched lazily on first need.
- Killed on resource pressure (LRU within quota class, or
  rss/cpu cap exceeded).
- Restarted on schedule if `always-resident`.

Foreground UI talks to resident background via the same
Aqueduct mechanism the rest of the system uses — they are
just two processes in the same jail (or sibling jails). No
"service worker" programming model: it is a normal process
with a normal main loop. The reason service workers needed
weird semantics (no DOM, no global state) was that they ran
inside the browser process; that constraint is gone.

### 11.4 Triggered background

System delivers named events; app declares the entry point
to exec on each.

| Event | Source |
|---|---|
| `push` | Push notification via system Tabellarius (§11.5) |
| `alarm` | Scheduled time the app registered |
| `network-resume` | Connectivity returned |
| `tessera-changed:/path` | Watched namespace path mutated |
| `system-idle` | Device idle and charging |
| `boot-complete` | System finished booting |

System spawns a fresh jail, execs the declared entry,
delivers the event payload, waits up to `max-runtime`,
SIGKILL on exceed. No persistent state survives between
invocations except via the app's Tessera namespace.

This is `cron` + `inotify` + push handler unified into one
mechanism.

### 11.5 Tabellarius — the Tabellarius

A device-wide push relay daemon. **Tabellarius** (Latin:
*courier / letter-carrier*) is one daemon, all apps.

- App registers a public key with its publisher at install.
- Publisher's server pushes to the system's chosen relay,
  addressed by app identity + public key.
- Tabellarius decrypts, identifies the target app, **delivers
  via Aqueduct** as a typed `push` message:
  - If a resident background is running for the app, it
    sends the message on the app's existing Aqueduct
    connection. `SCM_CREDS` proves it came from Tabellarius.
  - Otherwise, it asks Portcullis to spawn the triggered-bg
    process per manifest (§11.4), then delivers once it is
    up.
- Tabellarius enforces isolation: app sees only its own
  pushes.

Delivery is **not** over per-jail loopback IPs even though
vnet jails have their own IPs. Per-jail IPs exist for
external networking (LAN discovery, server ports). Push is
local IPC and rides on Aqueduct, which already provides
faster transport, kernel-attested peer identity, and
typed-message framing.

Tabellarius is distinct from **Praeco** (user-facing
notification toasts + history): Tabellarius is the
*delivery* layer (remote → local app); Praeco is the
*display* layer (local app → user). Apps typically receive
a push via Tabellarius, then post a notification via
Praeco. Either can happen without the other.

The push relay is a network capability the user grants
**once**, at OS setup. Apps do not pick their own relay —
that is how the web ended up with N TCP connections per
device.

Strictly cleaner than Web Push: no per-app endpoint URLs,
no Google-as-default-routing, no per-app TCP fan-out.

### 11.6 Resource discipline

Background execution must not drain battery, disk, network
quota, or foreground responsiveness.

| Knob | Mechanism |
|---|---|
| CPU / RSS | `rctl` per jail |
| Network bytes | Broker (§4) meters and throttles |
| Disk | Tessera namespace quota |
| Scheduling priority | Scheduler `idle`-class by default for resident bg |

The manifest's declared limits intersect with system hard
limits; the *stricter* wins. The user sees aggregate
per-app energy/data dashboards (kernel accounting + broker
logs feed an Atrium app analogous to iOS's "Battery"
screen).

### 11.7 What this replaces from the web

| Web mechanism | Atrium equivalent |
|---|---|
| Service Worker `fetch` interception | Document viewer mediates network. |
| Service Worker `install`/`activate` | Normal app install/update flow. |
| Service Worker background sync | Triggered bg with `alarm` or `network-resume`. |
| Push API | Triggered bg with `push` via system broker. |
| Web Worker | Another process in the same jail. |
| SharedArrayBuffer / Atomics | Per-jail shared-memory capability (opt-in). |
| Notifications API | `atrium_notify_*` in platform library. |
| Wake Lock API | Foreground apps stay running while windowed; resident bg has its own lifecycle. |
| Periodic Background Sync | `alarm` event with declared cadence. |

Nine APIs collapse to "an app may have a resident process
and/or be wakeable by named events."

## 12. Addressing — names, manifests, content

Content addressing (`atrium-doc://<hash>`) gives integrity,
cacheability, archival. It does not give memorable names or
publisher iteration. **Nomenclator** (Latin: the Roman
household servant who whispered names to his master so he
could greet visitors) is the name-resolution service that
bridges the gap.

### 12.1 Three-layer resolution

```
example.com / weather                     ← human-readable
        ▼  (DNS-equivalent → publisher manifest URL + key)
publisher manifest, signed                ← stable contract
        ▼  (path lookup → current content hash)
atrium-doc://<hash>                       ← content-addressed
        ▼  (Tessera)
the bytes                                 ← integrity-verified
```

Three layers, each doing one thing.

### 12.2 Layer 1 — the name

Reuse DNS. Names like `weather.example.com` resolve via a
`TXT` record (or `_atrium.<name>` subdomain) pointing at:
- Publisher manifest URL.
- Publisher signing key fingerprint.

```
weather.example.com  TXT  "atrium-manifest=https://example.com/.well-known/atrium key=ed25519:abc..."
```

Replacing DNS is a separate problem outside this spec.
Inheriting it is the right v0 call.

### 12.3 Layer 2 — the publisher manifest

Signed CBOR (small, less footgun than JSON). Sketch:

```toml
publisher  = "example.com"
key        = "ed25519:abc..."
signed-at  = "2026-05-21T..."
expires-at = "2026-05-22T..."

[content]
"weather"                       = "atrium-doc://8f2a...c401"
"weather/seattle"               = "atrium-doc://3b71...91ef"
"weather/seattle?d=2026-05-20"  = "atrium-doc://5c12...7790"

[archive]
"weather" = ["atrium-doc://prev1...", "atrium-doc://prev2..."]
```

Manifest is itself content-addressed and cached.
Nomenclator fetches the current manifest per freshness
policy and verifies the signature.

Properties:
- **Verifiable** — publisher signs; tamper-detectable.
- **Replayable** — every named URL resolves to a specific
  hash; archival is structural.
- **Atomic rollover** — single signed manifest moves any
  number of paths at once.
- **Forever-cacheable bytes** — only the resolution
  invalidates, never the content.

### 12.4 Layer 3 — content

Resolved hash → Tessera → bytes. Already specified.

### 12.5 Link traversal

1. User activates `atrium-doc://example.com/weather`.
2. Nomenclator: DNS TXT for `example.com`, manifest URL +
   key.
3. Fetch manifest if not cached fresh.
4. Verify signature against key.
5. Look up `weather` path → content hash.
6. Tessera lookup. If absent, fetch *from anywhere*
   (publisher CDN, peer device, P2P), verify hash on
   receipt.
7. Document viewer renders.

**Step 6 is the win.** Because the address is the hash,
bytes can come from any source. HTTPS-as-trust-anchor is
not needed at the content layer; the publisher's signature
on the manifest is the trust anchor.

### 12.6 Properties that fall out

- **Same name, different bytes over time.** Publisher
  updates manifest. Old hashes remain valid; offline-cached
  clients still render the older version.
- **Specific historical content.**
  `atrium-doc://example.com/weather?at=2026-05-20` resolves
  through the manifest's `archive` section. Time-travel by
  design.
- **Publisher disappears.** Content survives in any cache
  that has it. The name resolves to "no current manifest,
  archived versions follow." Wayback-machine-as-default.

### 12.7 App URLs

Same structure, different scheme:

```
atrium-app://example.com/photos?path=/2026/sunset.jpg
```

Manifest entry resolves to "installed `com.example.photos`
app at entry-point Y." Uninstalled apps prompt for install
(after capability-manifest consent UI).

App manifest declares entry-point patterns:

```toml
[entry-points]
"/photos/album/{id}" = "open_album"
"/photos/photo/{id}" = "open_photo"
"/share-target"      = "receive_share"
```

Multiple apps may claim the same shape — user picks default,
same model as iOS Universal Links / Android intent filters.

### 12.8 What this does not solve

- **DNS itself.** Centralized, vulnerable to seizure.
  Replacing DNS is out of scope.
- **Discovery.** "Find articles about X" is search, an app
  problem, not an addressing problem. The platform does not
  ship search; the web's accidental search-as-default-UX is
  a URL-bar artifact we do not reproduce.
- **Phishing.** Visual confusables (`examp1e.com`) still
  work against a signed-manifest model. Hostname display
  and security indicators are a UI problem, not an
  addressing problem.

## 13. Identity and sign-in

### 13.1 First principles

Three concerns the web conflated:

1. **Authentication** — proving identity to a service.
2. **Identification** — a stable handle.
3. **Authorization scope** — what may be done on the user's
   behalf.

Browsers smashed these together because there was no
platform identity layer. Atrium has one.

### 13.2 OS as custodian, not identity provider

Atrium **deliberately does not run a federated OS account**
(no "Atrium ID" analogous to Apple ID / Google Account).
That class of system evolves into a rent-extraction surface
and lock-in mechanism.

The OS provides a **keychain** — a system service holding
cryptographic keys. Apps mint, use, and rotate keys via
capability-gated API. The user manages identities through
system UI. Federation, when it happens, is between
*services*, not via the OS.

Closer to `ssh-agent` / GPG than to the smartphone account
model.

### 13.3 Per-service keypairs

Each service the user signs into gets a **fresh keypair**,
minted on first sign-in:

1. Service requests sign-in via the Limen
   `sign-in` role (§10.3).
2. Vestibulum's sign-in UI (outside the app's jail) walks the
   user through persona choice, biometric/passcode, scope
   review.
3. Keychain mints ed25519 keypair specific to
   (persona, service).
4. Public key registered with the service; private key
   stays in keychain, never exposed to the app.
5. Subsequent sign-ins are challenge-response.

This is WebAuthn / passkeys done right: per-service keypair,
hardware-backed where available, no implicit shared identity.

### 13.4 Federated sign-in with unlinkable pseudonyms

For "sign in with $PROVIDER" patterns:

1. App A requests identity from provider P via Limen.
2. System UI: "Sign in to App A with P? P will see:
   <claims>".
3. User authorizes.
4. P issues a signed claim about the user, scoped to App A.
5. App A verifies P's signature against P's published key.

**Crucial property: the user's identity at P is not
exposed to App A as a stable cross-service identifier.**
P issues a per-relying-party pseudonym (BBS+ /
deterministic hash of `persona-id || service-id`). App A
gets a handle stable in its own context that does not link
the user across services.

Cross-service tracking via federated sign-in becomes
structurally impossible, not merely policy-restricted. The
web has the cryptographic primitives but does not ship the
unlinkable form by default because the federators benefit
from linkability. Atrium ships it as the default.

### 13.5 Sessions

Long-lived authenticated broker connections (§4) carry
session state; the keychain provides periodic
re-attestation. Sessions are first-class connection state,
not header bytes.

For REST-shaped APIs that need short-lived tokens, the
keychain mints scoped tokens on request (capability-typed:
"read /api/v1/foo until T"). Macaroon-style but minted
locally.

### 13.6 Multiple personas

A user has N personas (work, personal, throwaway). Each is
a separate keychain bundle. Vestibulum's sign-in UI prompts
persona on first interaction with a new service.

Persona switching is at the **system** level, not per-app,
because cross-persona linkage is the exact threat being
prevented.

This is what private browsing should have been: a real
separate identity, not a cookie-jar half-measure.

### 13.7 Recovery

Device-bound keys can be lost. Atrium's answer is honest:

- **Backup:** keychain encrypted with passphrase +
  biometric + recovery key, stored in the user's Tessera
  namespace.
- **Multi-device sync:** opt-in paired-devices model; no
  cloud account required, but the user may add a cloud
  relay as a sync path.
- **Recovery key:** printed/written at setup. Loss of all
  devices + loss of recovery key = identity lost.

The web pretends otherwise via email-based recovery — which
means the user's identity is actually their email
provider's identity. Atrium does not pretend.

### 13.8 What this replaces from the web

| Web mechanism | Atrium equivalent |
|---|---|
| Session cookies | Long-lived authenticated broker connections |
| OAuth 2.0 | `sign-in` embed role with unlinkable pseudonyms |
| OpenID Connect | Same, with claims via signed assertions |
| WebAuthn / passkeys | Default — per-service keypair, hardware-backed |
| "Sign in with Google" et al. | One option among many; OS pushes none |
| SOP for credentials | Per-service keychain entry; no cross-origin leakage by construction |
| `<input type="password">` | Vestibulum's sign-in UI handles credential entry; apps never see passwords |
| Third-party cookies | Do not exist. Cross-service tracking requires explicit consent. |

## 14. Updates and versioning

### 14.1 Mechanism

An update is a new signed bundle in Tessera. The publisher
manifest (§12) points at the new content hash. Install
flow:

1. Resolver notices the manifest pointer changed.
2. Fetch the new bundle by content hash.
3. Verify signature against the publisher's installed key.
4. Diff capability manifest against installed version
   (§14.2).
5. Atomic swap: app's installed-root pointer updates to
   the new bundle hash.
6. Resident background processes get SIGTERM with grace
   period; next launch uses the new binaries.

Steps 5–6 leverage Tessera's CAS shape: both versions exist
on disk simultaneously during the swap. No half-installed
state. Rollback is changing one pointer.

### 14.2 Capability diff consent

The user consented to a specific capability manifest at
install. An update that *adds* capabilities must re-prompt;
an update that drops or narrows them does not.

- **Auto-accept** strict subsets of the previous manifest.
- **Prompt** for additions, showing exactly what is new.
- **Refuse** updates that fail signed-by-same-publisher.

Enforced by content addressing: an update that lies about
its capabilities cannot install because the manifest is
part of the signed bundle.

### 14.3 Update timing classes

- **Automatic, in background** — trusted publisher, narrow
  diff, low-impact app.
- **Notified, deferred** — user sees diff, picks when to
  apply.
- **Required for next launch** — security update or
  capability change forces it before next run.

Publisher declares the *minimum* class; user can override
toward *more conservative* but never less. An app that
prefers frequent silent updates against the user's
preference simply does not update on their device until
they relent or uninstall.

### 14.4 Resident-app hot swap

Resident background processes update via graceful restart,
not in-process module swap. State flushed to Tessera,
process restarted, state restored. Same state-preserving
suspend/resume machinery as dev iteration (§9.3).

In-process hot module reload introduces version-skew
complexity (old state, new code) and is deliberately not
the production model.

### 14.5 Rollback

Trivial: change one pointer in the local install registry
back to the previous bundle hash. Bytes are still on disk
(Tessera CAS-FS does not reclaim until GC; user-installed
version is a pin). User-facing "revert to previous version"
is a real operation.

### 14.6 Publisher key rotation

The publisher manifest declares the signing key (§12.3).
Rotation:

- Publisher signs the new manifest with both old and new
  keys for a transition period.
- Resolver verifies either; clients see both keys until
  the old one expires.
- Old-key-signed manifests stop validating past the expiry.

For high-risk publishers (payment, identity providers), the
platform can pin a stable key; pinning changes require user
consent, same as capability additions.

## 15. Storage model

Each app has a Tessera namespace mounted at a known path in
its jail. This is the only persistent storage the app can
write to.

### 15.1 Sub-areas

```
/app                       — read-only, the installed bundle
/data                      — read-write, persistent, backed up
/cache                     — read-write, evictable, not backed up
/tmp                       — tmpfs, gone at shutdown
/shared/<channel>          — read-write, shared with other apps (cap-gated)
```

- `/app` — the bundle, mmap'd from Tessera CAS.
- `/data` — what the user cares about losing.
- `/cache` — performance-only; OS will evict under pressure.
- `/tmp` — ephemeral, per-process or per-session.
- `/shared/<channel>` — opt-in cross-app data sharing
  (§15.4).

### 15.2 Quotas

Declared in the manifest (§5.1):

```toml
[storage]
data  = "100MB"   # backed up
cache = "1GB"     # evictable
```

Enforced by Tessera namespace quotas. The OS *will* evict
`/cache` on disk pressure; apps must treat it as a soft
hint.

### 15.3 Backup

`/data` is included in backups; `/cache` and `/tmp` are
not. Apps do not manage backup themselves — they place data
correctly, the OS handles backup against the user's chosen
target. Declarative analogue of iOS's
`NSURLIsExcludedFromBackupKey`.

### 15.4 Cross-app sharing — named channels

The web ties storage scope to identity (cookies / localStorage
per origin), which breaks for many cases (multiple apps from
one publisher; apps that should share data without sharing
identity).

Atrium answer: explicit named channels.

```toml
[shared.export]
"com.example.photos.library" = { mode = "read-write" }

[shared.import]
"com.example.photos.library" = { mode = "read" }
```

App A exports; App B imports. The system mediates: B sees
A's exported subtree at `/shared/com.example.photos.library`.
Capability-gated; user consents to the link at install or
via powerbox runtime grant.

No origin-as-trust-boundary, no "same publisher" tests.
User controls who sees what.

### 15.5 Sync

Opt-in capability per app:

```toml
[sync]
enabled = true
target  = "user-default"
```

System sync service handles replication; app's `/data`
becomes the synced subtree. Apps do not implement sync
themselves.

Conflict resolution: CRDT-based at the Tessera level where
possible; per-app conflict-resolution capability for cases
where structure matters (calendar collisions, document
concurrent edits).

### 15.6 What this replaces from the web

| Web mechanism | Atrium equivalent |
|---|---|
| Cookies | Per-service keychain entries (§13); no app data in headers |
| `localStorage` | `/data` |
| `sessionStorage` | `/tmp` or process memory |
| IndexedDB | `/data` + SQLite-or-similar in the platform library |
| Cache API | `/cache`, evictable |
| File System Access API | Scrinium grants per-file fds |
| Origin Private File System | `/data` — already origin-equivalent by jail |
| Shared Web Workers | Resident background process |
| BroadcastChannel | Aqueduct pub/sub |
| Storage Access API (third-party cookies) | Named shared channels with explicit consent |

## 16. Forms and autofill

The web has decades of "autofill" pain: every site rolls its
own form layout, browser heuristically guesses which input is
"first name," password managers fight with site JavaScript,
saved credit card numbers leak across origins.

Atrium answer: extend powerbox (§5.2) to **data items**.

### 16.1 Loculus — the wallet service

**Loculus** (Latin: *small carried purse / box for
valuables*) is a system service holding user-curated data
items:

- Addresses (with labels: Home, Work, …).
- Payment methods (card or device-bound payment token).
- Saved form profiles.
- Generated identities (for one-off "create account" flows).

Loculus UI is system-trusted, outside any app's jail.

### 16.2 Autofill flow

1. App requests `autofill` via Limen with a type
   filter (`address`, `payment`, …).
2. Loculus overlays the app's window (Pergola surface
   owned by Loculus, not the app).
3. User picks one item.
4. **Only the picked item** is delivered to the app via a
   typed message; the rest of Loculus remains invisible.

The app never reads Loculus. The app never sees items the
user did not pick. Same trust shape as Scrinium (file
picker).

### 16.3 Credentials are not in Loculus

Passwords and passkeys live in the keychain (§13), not in
Loculus. Sign-in flows go through Vestibulum's sign-in UI
(§13.3), which never reveals credentials to the app.

The split (credentials in keychain managed by Vestibulum;
structured user data in Loculus) keeps the highest-risk
data on its own management path.

## 17. Media, codecs, DRM

### 17.1 Codec capability

Common codecs (H.264, H.265, AV1, VP9, Opus, AAC, FLAC) are
a platform service accessible by all apps via the platform
library. Hardware-decode where the device supports it; CPU
fallback otherwise.

Exotic codecs are libraries the app links into its bundle.
Decode runs in the app's jail; no platform involvement.

### 17.2 DRM — opt-in capability, not platform mandate

DRM (hardware-attested decryption for protected content) is
an explicit capability requiring a signed publisher → device
attestation chain. Apps that genuinely need it (streaming
services with content-licensing constraints) declare:

```toml
[capabilities]
drm-attestation = ["widevine-l1", "fairplay"]
```

The capability gates access to a hardware-backed crypto
service. The service performs the attestation handshake and
returns a decrypted stream the app can render but not
reroute.

**The platform does not mandate DRM.** Atrium does not ship
an EME-equivalent that every browser must support. Apps
without the capability simply cannot decrypt DRM-protected
streams; apps that need it install the capability and the
chain at publisher discretion.

The honest position: DRM is a service some apps want and
some users tolerate. The OS provides the mechanism; the
ecosystem chooses whether to use it.

### 17.3 Encrypted-media playback

The standard pipeline for the (rare) DRM case:

```
publisher's CDN → encrypted bytes → app
                                     │
                                     ▼
              hardware crypto service (attested)
                                     │
                                     ▼
              composited surface (no app readback)
```

The decrypted bytes never appear in app memory; only in the
hardware path through to the compositor's secure-surface
slot. Pixel-readback rules from §10.3.3 apply.

## 18. Device access

Camera, microphone, geolocation, accelerometer, ambient
light, etc.

All powerbox-mediated. No ambient "this app may use the
camera at any time" capability.

### 18.1 Capture devices (camera, microphone)

System UI presents a "record" affordance overlaid on the
app's window (Pergola surface owned by the system, like
autofill in §16.2). User taps to start; an fd to the
capture stream is delivered to the app. Stopping is
system-mediated; the indicator (recording icon, system
sound) is non-spoofable.

Session-bounded: closing the affordance terminates the
stream. Background capture requires a separate, loudly
disclosed capability and shows a persistent system
indicator.

### 18.2 Geolocation

Two flows:

- **One-shot share:** user invokes system location picker,
  optionally adjusts pin, confirms. App receives one
  location.
- **Session grant:** for navigation-shaped apps. User
  authorizes a session with a visible system indicator and
  an explicit timer. Session ends on app close or timer
  expiry; no "always" tier without manifest declaration +
  install-time consent.

### 18.3 Sensors

Accelerometer / gyroscope / barometer / etc. — fall into
two classes:

- **High-rate motion sensors** require a capability,
  because they leak surrounding activity (keystrokes,
  speech, location via dead reckoning).
- **Coarse-resolution sensors** (ambient light, battery,
  etc.) are available by default; rate-limited.

The platform classifies; manifests declare; powerbox
mediates for runtime sensitive grants.

## 19. Peer-to-peer networking — Concursus

WebRTC's value is browser-mediated peer connections with
NAT traversal and signaling consent. Insula's answer:
**Concursus** (Latin: *a coming-together*), a system service
analogous to Tabellarius (§11.5) but for symmetric
device-to-device channels.

### 19.1 Architecture

```
app A (device 1)    Concursus       Concursus    app B (device 2)
       │              (system)        (system)             │
       │  request_peer(role=…, B's pub-key)               │
       ├─────────────►                                    │
       │              │  signaling via known relay        │
       │              ├──────────────────────────────────►│
       │              │                                   │
       │              │  STUN/TURN-equivalent             │
       │              │  NAT traversal                    │
       │              │                                   │
       │  channel established (typed role)                │
       │◄═════════════╪══════════════════════════════════►│
```

### 19.2 Capability shape

Apps declare peer roles in manifest:

```toml
[peer.implements]
"file-share" = "..."

[peer.requests]
"file-share" = "..."
```

Concursus matches roles; both sides must declare the role
to establish a peer.

### 19.3 Trust and consent

User-visible consent prompt at peer establishment time —
"Connect to <peer identity> for <role>?" The connecting
peer's identity is the device's attested identity (§13.2);
unknown peers prompt loudly.

Channels carry typed role messages, same shape as embed
roles (§10.3.2). No raw byte streams unless `raw-peer`
capability is declared and consented to.

### 19.4 Signaling and relays

Signaling rides on a system-chosen relay (analogous to the
push relay). User picks at setup; defaults to a public
free-tier relay; can be self-hosted.

The relay sees encrypted signaling traffic only — it cannot
read peer messages, which are end-to-end encrypted with
per-channel keys derived from device identities.

## 20. Distributed apps and remote rendering

The web's "server-side" story is several distinct things
mashed together. Insula separates them, and one of them —
remote app execution with local rendering — becomes a
first-class capability of Aqueduct + Fresco rather than
needing a separate protocol stack (X11, RDP, VNC, Citrix).

### 20.1 What "server-side" actually contains

| Web concept | What it really is | Insula's answer |
|---|---|---|
| Server-rendered HTML (PHP, Rails) | Avoid shipping JS to do first paint | Does not apply — no JS to ship, no HTML to render; AOT native code mmap's instantly |
| Hybrid SSR + hydration (Next.js, Leptos SSR) | Bridge between document and app modes of the web | Does not apply — apps-vs-documents split is clean (§0.5); hybrid is unnecessary |
| Remote app execution (X11, RDP, Citrix) | Run code on machine A, show UI on machine B | First-class (§20.2). Aqueduct is already a network-transparent substrate. |
| Server-side business logic (any backend) | Hold data + serve API | Just an Aqueduct service over the network (§20.5) |

The first two are properties of the web's document/app
mongrel shape, deleted in §0.5. The third and fourth are
the substantive ones.

### 20.2 Remote app execution — X11 done right

Aqueduct is OS-agnostic and network-transparent by design.
Fresco is a retained-mode scenegraph protocol. Together,
they support **remote app execution as a normal Insula
deployment shape**:

- The remote app is a normal Insula app, running in a
  Portcullis jail on the server.
- Its Aqueduct client connects to *the user's* compositor
  via a network-traversing Aqueduct.
- Fresco scenegraph commands flow over the network instead
  of via unix socket.
- The local compositor renders them.
- Input events flow back the other way.

The user sees a window on their desktop. The window is
backed by:
- A server in the closet, or
- A datacenter VM, or
- Their phone (continuity-style hand-off), or
- A friend's machine (collaborative session).

This is **strictly cleaner than X11 / RDP / Citrix**:

- The scenegraph protocol (Fresco) is designed for IPC
  compression and content-addressed assets, not pixel
  bashing. Textures and meshes Tessera-dedup across the
  network so cold first frame is the only worst case;
  subsequent frames are scenegraph deltas.
- The transport (Aqueduct) is designed for this — not a
  layer-violating retrofit like X11-over-SSH.
- Jail boundaries + capabilities transfer cleanly — the
  remote app's manifest is checked against what the user
  authorized for *remote* execution; the consent UI shows
  remote-execution as its own dimension.
- Identity via Vestibulum's per-service keypairs (§13)
  means the remote app authenticates to the user and the
  user to the remote app using the same machinery as a
  local app — no SSH-key / VNC-password split.

### 20.3 Trust and consent for remote execution

Running an app remotely is a **separate trust statement**
from "I installed this app locally." The remote-execution
consent prompt shows:

- Identity of the remote host (Vestibulum-attested).
- What capabilities the remote app holds *on the server*
  (its own jail manifest), distinct from what the user
  grants for the rendering session.
- What the user shares with it during the session (input
  events, clipboard, any attached fds).

Capability flow is asymmetric:
- The remote app **does not** automatically receive the
  user's local capabilities (storage, peripherals,
  contacts).
- The user can choose to grant specific local capabilities
  to the remote session via powerbox (e.g., "share this
  file with the remote session" via Scrinium).

This is iOS Universal Control / Continuity-style design,
generalized.

### 20.4 Costs and limits

Honest tradeoffs:

| Concern | Effect | Mitigation |
|---|---|---|
| Latency | Local Fresco is sub-ms; network adds RTT. ~20 ms is fine; ~100 ms felt; ~300 ms breaks interactivity | LAN deployments work great; transatlantic interactive is painful (same as any thin-client) |
| Bandwidth | First frame cold = full asset upload | Tessera dedup; assets pinned and reused on subsequent frames |
| Reliability | Network partition → app appears frozen | Compositor surfaces `child_lost`-equivalent state (cf. §10.3.5) with last-frame-stale or placeholder |
| Trust | User trusts server with whatever the app handles | Distinct consent dimension (§20.3); per-session capability grants via powerbox |

### 20.5 Server-side business logic — just network IPC

A traditional PHP / Rails / Django / Node backend is "a
stateful server that holds data and serves requests." On
Insula:

- The server runs Atrium (or any OS that ships Aqueduct).
- The "backend" is an Aqueduct service in a Portcullis
  jail.
- Clients connect over the network; same `atrium_net_*`
  plumbing (§2.3, §4) as any network call.
- Data lives in Tessera, which works identically server-
  side and client-side.

The serialization boundary — HTTP + JSON + the
TypeScript/Python/Ruby polyglot mismatch — **disappears**.
Client and server speak Aqueduct typed messages; one
program can have parts in both places, in the same
language, talking over typed channels.

The "REST API" layer becomes optional: use it for interop
with non-Atrium services; do not use it as the universal
coordination model.

### 20.6 Specific framework collapses

- **PHP / Rails / Django** — their job (template-driven
  HTML generation) does not exist in Insula. Authors write
  an Insula Aqueduct service that responds with typed
  data; clients render via Pergola. No HTML in the
  pipeline.
- **Next.js / Remix / SvelteKit** — exist to paper over
  the document/app mongrel. Insula's clean split (§0.5)
  removes the problem; these frameworks have no analogue
  because they have nothing to bridge.
- **Leptos SSR / Phoenix LiveView / similar hybrids** —
  interesting because they already approached the
  "one-language-both-sides" vision. On Insula the
  SSR/hydration dance collapses entirely: one program,
  parts on the server, parts on the client, talking via
  Aqueduct in the same language. The framework gets
  *simpler*, not more complex.
- **GraphQL / REST schema layers** — replaced by Aqueduct
  typed messages (same role: define the wire shape between
  client and server).
- **WebSocket / SSE / long-polling** — replaced by Aqueduct
  pub/sub on a network-traversing connection.

### 20.7 What does NOT collapse

The web's client/server split was a workaround for two
distinct things:

1. **You cannot run untrusted code on the client** → run
   it on the server.
2. **The client cannot be trusted with shared data** →
   keep the data on the server.

Insula changes (1) — you *can* run untrusted code locally,
because it is jailed. Insula does **not** change (2) for
*shared* data. A social network's posts, a payment ledger,
a global search index, a multiplayer game's state — these
inherently live on a server because they are not the
property of any one user. They remain server-shaped in
Insula; they just speak Aqueduct over the network instead
of HTTP+JSON.

So the right framing is:

- **Single-user data** (your photos, your notes, your
  documents) — lives locally in Tessera. Sync (§15.5) is
  opt-in.
- **Multi-user / platform-owned data** (social graph,
  global index, shared game state) — lives on a server.
  Accessed via Aqueduct over network.

The "server" never goes away for the multi-user case. It
stops being a *language boundary* or a *rendering boundary*
— it is just a *trust-and-data-location* boundary.

### 20.8 Implications for Aqueduct itself

The properties this section assumes:

- Aqueduct authenticates remote peers via Vestibulum-
  attested device identities + per-service keypairs (§13).
- Aqueduct provides TLS-class confidentiality and
  integrity on network hops.
- Aqueduct connection establishment surfaces "remote
  endpoint" cleanly to the user (no transparent-but-
  surprising network calls).
- Aqueduct error / disconnect signaling is rich enough for
  the compositor to render meaningful UX on partition.

These are properties Aqueduct must commit to; they are
already part of its stated design but worth pinning here so
the Insula spec's dependency is explicit.

## 21. Notifications — Praeco

User-visible notifications are handled by **Praeco** (the
existing Atrium notifications service — see NAMING.md).
Insula's contribution is the contract Insula apps use when
calling Praeco, and the interaction with Tabellarius (§11.5)
for push-driven notifications.

### 20.1 Posting

App posts a typed notification via Praeco:

```c
atrium_praeco_post(.title=..., .body=..., .actions=[...],
                   .urgency=PRAECO_URGENCY_DEFAULT,
                   .group=..., .replaces_id=...);
```

`actions` are declared in the manifest and resolve to
triggered-bg events or deep-link entry points (§12.7).

### 20.2 User interaction

Tap on notification → activates declared action. The
foreground app launches at the declared deep-link, or the
triggered-bg event fires. Notification dismissal is a
silent system event the app may subscribe to.

### 20.3 User control

System UI shows per-app notification settings: urgency
filters, group muting, do-not-disturb interactions, quiet
hours. App-level controls live outside the app.

Standard mobile-OS shape; nothing exotic.

## 22. Internationalization

Locale, layout direction, font fallback, calendar, and
number formatting are **system-wide settings** inherited by
all apps via the platform library and Pergola.

- Apps query the current locale; layout primitives in
  Pergola handle RTL/LTR mirroring without per-widget
  opt-in.
- Font fallback walks a system-managed font stack; missing-
  glyph rendering never produces tofu silently in a
  certified app (Pergola flags it).
- AX names (§10.4) follow the same locale path as visible
  text.

Detail lives in the Pergola spec; this design only commits
to "locale is a system property apps inherit," not to the
specific Pergola API.

## 23. Scale and dedup

The "I have 500 apps installed" question. The web's answer:
every site re-downloads its JS/CSS/assets, partially saved
by HTTP caching that misses constantly. Native app
ecosystems (iOS, Android) are better — but still pay full
size per app.

Atrium's answer is **Tessera's content-addressed dedup**:

- Chunk-level dedup: same library shipped by two apps
  shares its bytes on disk.
- `tessera-binsplit` (function-level dedup, on the
  roadmap): same function compiled by two apps shares
  bytes even if surrounding code differs.
- Shared assets (system fonts, common icons) are pulled
  from a system-namespace once, used by all apps.

Per-app marginal install cost is dominated by the truly-
unique bytes, not the package size. 500 apps on Atrium take
substantially less disk than 500 apps on a non-dedup OS.

The user-facing surface: install-size reports show *unique
bytes* and *shared bytes*, so the user understands the
actual cost of an install.

## 24. Threat model

What follows is the explicit "what can attackers do, what
they can't, what's residual risk" pass. Spec hardening, not
new design.

### 24.1 Adversary classes

| Class | Capability |
|---|---|
| Malicious publisher | Authors a signed bundle they intend to misuse |
| Compromised publisher | Legitimate signing key stolen, attacker pushes malicious update |
| Network adversary | Observes / tampers with traffic between device and publisher |
| Co-resident app | Other installed app on the same device |
| Local user with physical access | Has device in hand |
| Hardware attacker | Side channels, fault injection, supply chain |

### 24.2 What the design defends against

**Malicious publisher** — limited by capabilities. App can
only do what the manifest declared and the user accepted at
install. Powerbox confines the rest. Damage is bounded.

**Compromised publisher** — capability-diff consent (§14.2)
blocks silent expansion. Update introducing new caps stops
on the user's review. Key pinning (§14.6) protects
high-risk apps. Damage is detectable and reversible
(rollback, §14.5).

**Network adversary** — content-addressed bundles + signed
publisher manifests (§12) mean tampering is detectable.
Confidentiality of in-transit content is the publisher's
TLS responsibility; the platform's trust anchor is the
publisher's signing key, not the CA system.

**Co-resident app** — jail boundary (§4.1) is the security
boundary. Cross-jail composition (§10.3) preserves
isolation; pixel readback prohibited; AX composition
mediated. Cross-service tracking prevented by per-service
keypairs (§13.3) and unlinkable federated pseudonyms
(§13.4). Side channels (timing, cache) bounded by §10.3.6
mitigations and per-jail GPU contexts.

**Local user with physical access** — outside this spec's
scope. Disk encryption, screen-lock, biometric, recovery
keys are platform concerns covered elsewhere.

**Hardware attacker** — out of scope. Atrium relies on the
underlying FreeBSD + hardware security primitives.

### 24.3 What the design does NOT defend against

Honesty matters more than reassurance:

- **Phishing** at the name layer (visual-confusable hosts,
  §12.8). UI mitigations help; cryptography does not.
- **Social engineering** through powerbox prompts — a user
  who clicks "yes" on every prompt can be tricked. UI
  design and prompt phrasing matter; the model is "make it
  obvious," not "make it impossible."
- **Side channels we have not enumerated.** Timing,
  cache, power, EM, acoustic — Atrium does what current
  best practice requires (per-jail GPU contexts, no
  shared-CPU SMT siblings between jails when feasible)
  but cannot promise immunity.
- **Implementation bugs.** Any of the trusted system
  services (Portcullis, Limen, Tabellarius, Concursus,
  Loculus, Vestibulum, Nomenclator, Praeco, compositor) is
  a source of vulnerability if implemented incorrectly.
  Threat model design must be backed by implementation
  rigor; this spec does not promise the latter.

### 24.4 Trusted system surface

The trusted-computing-base for this design:

- FreeBSD kernel.
- Atrium-specific kernel modules (jail extensions, GPU
  ABI).
- Portcullis daemons.
- Aqueduct + Castellum.
- Fresco compositor.
- Pergola toolkit (only for AX correctness and embed slot
  policies; apps that use other toolkits are still
  sandboxed correctly).
- Insula services: Limen, Tabellarius, Loculus, Concursus,
  Nomenclator.
- Adjacent Atrium services Insula leans on: Vestibulum
  (sign-in + keychain), Praeco (notifications), Scrinium
  (pickers), Opifex (bundle install/update), Tabula
  (clipboard).
- Network capability broker (`atrium-netd`).

Anything outside this list is untrusted. App code is
untrusted regardless of language, signature, or publisher.

### 24.5 Capability hygiene principles

For each capability the platform might add:

1. **Default-deny.** Manifest must declare; install must
   consent.
2. **Powerbox for intermittent grants.** Don't ask the user
   for ambient authority; mint per-use capabilities at
   point of use.
3. **Loud indicators for ambient grants.** If a capability
   is in effect (recording, location), the system indicator
   must be visible and non-spoofable.
4. **Scope to the smallest useful unit.** Per-host network,
   per-file storage, per-service identity.
5. **Time-bound where possible.** Sessions expire;
   re-grants are cheap; "forever" requires explicit
   manifest declaration.
6. **No reach-through.** A capability granted to an app
   does not transitively grant it to embedded children;
   each jail's caps are its own.

## 25. Open questions

- **macOS host adapter scope and design.** §0.7 fixes
  the strategy (macOS-first bring-up) and the abstraction
  (host adapter). The actual implementation of the macOS
  host adapter — sandbox profile generation from manifests,
  launchd integration, Network.framework broker, Keychain
  bridge to Vestibulum — needs its own spec. Likely
  `docs/spec/insula-host-macos.md`.
- **IR format precise choice.** WASM is the realistic pick;
  the exact link-shim shape between WASM module imports and
  `libatrium.so` needs spec work.
- **A11y wire format.** §10.4 locks the structural decisions;
  the wire-level Aqueduct message format for tree snapshots,
  subscription updates, and inbound activation requests
  still needs spec work. Likely lives in a sibling
  `docs/spec/atrium-ax.md`.
- **Document authoring-format details.** §10.6 fixes the
  multi-format strategy; the precise Markdown-superset
  extensions and the HTML+CSS subset's bounded layout-
  primitive set need spec work.
- **App deep-link / URL equivalent.** Need a stable
  addressing scheme for "open this app at this position."
  (Documents have it via content-addressed
  `atrium-doc://<hash>#anchor`; apps need an analogue.)
  Manifest-declared entry points are the obvious shape;
  details pending.
- **Publisher-manifest CBOR schema (Nomenclator).** §12
  fixes the three-layer resolution shape; the precise CBOR
  field layout, freshness rules, and key-rotation semantics
  need spec work. Likely lives in a sibling
  `docs/spec/nomenclator.md`.
- **Limen role catalogue.** §10.3 fixes the architecture;
  the initial role catalogue, the wire format for typed
  messages, and Limen's manifest surface still need spec
  work. Likely a sibling `docs/spec/limen.md`.
- **Tabellarius wire / relay protocol.** §11.5 fixes the
  delivery semantics; the wire format between publisher
  server → relay → Tabellarius and the relay-discovery /
  user-choice UX need spec work. Likely
  `docs/spec/tabellarius.md`.
- **Loculus item schema.** §16 fixes the data-item powerbox
  pattern; the concrete item schemas (address, payment,
  profile) need spec work. Likely
  `docs/spec/loculus.md`.
- **Concursus signaling / NAT-traversal.** §19 fixes the
  shape; the actual STUN/TURN-equivalent protocol and
  relay-discovery story need spec work. Likely
  `docs/spec/concursus.md`.
- **Keychain naming.** Currently described descriptively as
  part of Vestibulum's responsibility (§13). Open question
  whether it deserves its own Latin name (candidate:
  *Clavarium*) or stays an internal aspect of Vestibulum.
- **Network broker policy expressiveness.** Hostname + port +
  proto is the floor; TLS pinning, allowed methods, allowed
  paths are reasonable additions; the line between policy and
  app-internal logic needs to be drawn.
- **Cross-arch fat-binary tooling.** If publishers ship native
  for multiple arches, bundle format needs a slice picker
  analogous to Mach-O fat headers.

## 26. References

### Atrium services Insula depends on

- `docs/NAMING.md` — canonical naming reference for the
  Atrium platform.
- `docs/spec/portcullis.md` — jail launcher and capability
  enforcement (this spec's enforcement layer).
- `docs/spec/pergola.md` — toolkit (this spec's UI layer).
- `docs/spec/fresco-rendering-stack.md` — compositor.
- `docs/spec/aqueduct.md` — IPC substrate.
- `docs/spec/stoa.md` — persistent session service.
- `docs/spec/tessera-fs.md` — content-addressed FS.
- `docs/spec/atrium-pkg.md`,
  `docs/spec/atrium-pkg-registry.md` — Opifex packaging and
  registry (overlap with §3 / §14 to be reconciled).
- `docs/spec/atrium-netd.md` — network capability broker.

### Sibling Insula component specs (planned, not yet written)

- `docs/spec/limen.md` — embed broker + role catalogue.
- `docs/spec/tabellarius.md` — push delivery protocol.
- `docs/spec/loculus.md` — wallet schema.
- `docs/spec/concursus.md` — peer signaling.
- `docs/spec/nomenclator.md` — name resolution protocol.
- `docs/spec/atrium-ax.md` — accessibility wire format
  (referenced from §10.4).

### Explicitly NOT used by Insula

- `docs/spec/tier2-renderer.md`,
  `docs/spec/tier2-shader-codegen-constraints.md` — bespoke
  shader codegen (specialized for shaders; not an app-
  scripting backend).
