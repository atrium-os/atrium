# Insula host adapter — macOS

Status: design sketch (pre-implementation).
Last updated: 2026-05-21.

This document specifies the **macOS host adapter** for
Insula — the per-OS implementation that satisfies Insula's
OS-agnostic contract (`insula.md` §0.7) using macOS-native
primitives.

The bring-up sequence (`insula.md` §0.7.3) makes this the
**Phase 1 deliverable**: get Insula running on macOS first,
to (a) validate the contract abstractions, (b) lower
adoption activation energy, and (c) build the developer
audience that will then drive Linux / Windows / Atrium
adoption.

## 0. Position

### 0.1 What this spec covers

Per `insula.md` §0.7.2, the *host adapter* is the per-OS
slice that implements:

| Insula concept | macOS implementation |
|---|---|
| Sandbox boundary | App Sandbox / Sandbox.kext + entitlements |
| Service launch | launchd + sandbox profiles |
| Capability enforcement | sandbox profile + entitlements |
| Per-app networking | Network.framework + broker |
| Identity / keychain | macOS Keychain wrapped behind Vestibulum API |
| Resource limits | sandbox limits + `setrlimit` + Foundation memory pressure APIs |

The userspace services (Limen, Tabellarius, Loculus,
Concursus, Nomenclator, atrium-ax) are **the same code
on every host**. This spec is exclusively about the
bottom-of-stack adapter — about 5–10 KLoC, mostly Objective-
C / Swift glue.

### 0.2 What this spec does not cover

- Higher-level Insula services (sibling specs cover
  those).
- The Pergola UI toolkit — Pergola has its own macOS
  backend (Metal via MoltenVK; covered in
  `docs/spec/pergola.md` and the existing Fresco macOS
  work).
- macOS distribution mechanics (App Store vs.
  Developer ID vs. signed-by-Atrium-team) — covered
  in §10.

### 0.3 Scope of "macOS"

Target: macOS 14+ on Apple Silicon (the existing
Atrium bring-up environment per memory).
Intel Macs: optional; same code, different codegen
target.

## 1. Architecture

### 1.1 Process model

Insula on macOS runs as a set of regular macOS processes,
each in its own App Sandbox container:

```
.insula-host (system service)             ; runs as user session daemon
├── portcullis-shim                       ; macOS adapter for jail/launch
│   └── spawns jailed children via launchd or NSTask
├── limend, tabellariusd, loculusd, etc.  ; Insula system services
├── Vestibulum-macos-bridge               ; macOS Keychain ↔ Vestibulum
└── atrium-netd-macos                     ; network broker

User-facing apps:
├── Artifex.app                          ; one App Sandbox container
├── (other Insula apps)
```

`.insula-host` is itself a single signed macOS app that
hosts the system daemons. It runs as a per-user agent
(`~/Library/LaunchAgents/...`), starting at login.

### 1.2 Privilege model

`.insula-host` runs at user privilege, not root. This is
deliberate — macOS App Sandbox does not require root,
and avoiding root simplifies install and audit.

Some Insula primitives need privilege macOS does not
grant to user-level apps:
- Raw network sockets: not available; broker uses
  `Network.framework` user-level instead.
- Arbitrary filesystem access: not available; mediated
  via security-scoped bookmarks (§5).
- Inter-process ptrace for debugging: Apple's
  `task_for_pid` model; requires entitlement.

These constraints are *features* — they prevent the
host adapter from being more permissive than macOS
itself allows.

### 1.3 What replaces FreeBSD jail

On Atrium, Portcullis builds a FreeBSD jail. On macOS,
the equivalent boundary is the **App Sandbox container**:

- Each Insula app launches in its own sandboxed
  container (`~/Library/Containers/<bundle-id>/`).
- Sandbox profile is generated from the app's Insula
  manifest at install time.
- Entitlements specify capability bits (Network.client,
  files.user-selected, microphone, etc.) declared by
  the manifest.

App Sandbox is weaker than a FreeBSD jail in some
respects (less kernel-strict; some macOS APIs bypass)
but stronger in others (integrated with macOS's
security model, including TCC for privacy-sensitive
capabilities). It is **sufficient** for the Insula
contract; the differences are documented in §11.

## 2. Manifest → sandbox profile + entitlements

The core of the host adapter: translating Insula
manifests into macOS security descriptors.

### 2.1 Mapping table

| Insula manifest field | macOS implementation |
|---|---|
| `[network] hosts = [...]` | `com.apple.security.network.client` entitlement + atrium-netd-macos enforces hostname allowlist |
| `[network] inbound = false` | absence of `com.apple.security.network.server` |
| `[storage] data = ".../"` | App Sandbox container at `~/Library/Containers/<bundle-id>/Data/` |
| `[storage] cache = ".../"` | `~/Library/Containers/<bundle-id>/Data/Library/Caches/` |
| `[input] keyboard = "focus"` | implicit; sandbox does not need extra |
| `[input] camera = ...` | TCC prompt at first use + `com.apple.security.device.camera` entitlement |
| `[input] microphone = ...` | TCC + `com.apple.security.device.microphone` |
| `[ipc] services = ["fresco-protocol", ...]` | XPC service whitelist (`com.apple.security.temporary-exception.mach-lookup.global-name`) |
| `[compute] cpu, rss, wall` | NSProcessInfo + `setrlimit` + AppNap-aware QoS classes |
| `[render] fresco = true` | Pergola macOS backend permissions |
| Powerbox-granted resources (file picker output, etc.) | Security-scoped bookmarks |

### 2.2 Sandbox profile generation

The portcullis-shim generates a `.sb` profile (SBPL —
Apple's sandbox profile language) from the manifest at
install. Example skeleton for a typical app:

```
(version 1)
(deny default)

;; Standard Insula app permissions
(allow process-fork)
(allow process-exec (literal "/Applications/InsulaHost/bin/atrium-app-loader"))

;; Container access
(allow file* (subpath (param "CONTAINER_DIR")))

;; XPC to allowed Insula services
(allow mach-lookup (global-name "atrium.aqueduct"))
(allow mach-lookup (global-name "atrium.limen"))
;; ... more based on [ipc] services declared

;; Inherited from entitlements: network, devices, etc.
```

The exact SBPL is internal to the host adapter; the
manifest is the canonical truth.

### 2.3 Entitlements

Standard macOS entitlements are added based on
manifest declarations:

```xml
<dict>
  <key>com.apple.security.app-sandbox</key>
  <true/>
  <key>com.apple.security.network.client</key>
  <true/>
  <key>com.apple.security.files.user-selected.read-write</key>
  <true/>
  <!-- ... -->
</dict>
```

The entitlements file is signed alongside the app
binary as part of the install flow (§10).

## 3. Service launch

### 3.1 launchd integration

Insula system services (Limen, Tabellarius, etc.) are
registered as launchd agents under
`~/Library/LaunchAgents/atrium.<service>.plist`.

`.insula-host` ships these plists at install. They
start at login, stop at logout, and respect macOS
power-management heuristics (AppNap, throttling under
battery, etc.).

### 3.2 User-app launching

When Limen or another orchestrator needs to launch a
sandboxed Insula app:

```
1. Resolve manifest → bundle path + entitlements.
2. NSTask spawn with sandbox profile (or use
   posix_spawn with sandbox flags via _spawnattr_init).
3. Pass embed handles / Aqueduct fds via launchd's
   sockets-passing or Mach-port bootstrap.
4. Child app calls Aqueduct's macOS connect helper to
   pick up its inherited fds.
```

### 3.3 Pool of pre-launched empty containers

For the hover-preview UX (`insula.md` §8), Insula's
shared jail pool maps to a pool of pre-spawned
sandboxed processes blocked on a Mach port, ready to
exec into a specific app on demand.

The pool size (~8 processes) costs a few MB resident
total; same shape as the Atrium implementation, just
the underlying primitive is different.

## 4. Network capability broker

`atrium-netd-macos` is the macOS-specific implementation
of the network broker (`insula.md` §4.2).

### 4.1 No raw sockets

App Sandbox apps cannot create raw sockets. The broker
uses `Network.framework` (modern macOS userspace
networking) for outbound connections; the broker
itself is a service the app talks to via an Aqueduct
channel.

### 4.2 Hostname enforcement

The broker:
1. Receives `connect(host, port, proto)` requests from
   apps via Aqueduct.
2. Checks the requesting app's manifest hostname
   allowlist (cached locally; refreshed when manifests
   change).
3. Resolves DNS via `Network.framework` (modern macOS
   recursive resolver; supports DoH if user-configured).
4. Opens the connection on app's behalf.
5. Returns the resulting fd to the app.

### 4.3 What the broker enforces

- Hostname/port/protocol allowlist from manifest.
- TLS pinning if declared.
- Per-host rate limiting (configurable, default
  generous).

### 4.4 What the broker does NOT do

- TLS termination (apps do their own TLS over the
  delivered fd).
- Content inspection (privacy by design).
- VPN-class routing (apps can opt into a separate VPN
  capability if needed).

## 5. Filesystem and Tessera

### 5.1 Per-app storage

Each Insula app's `/data`, `/cache`, `/tmp` (per
`insula.md` §15.1) map to:

| Insula path | macOS path |
|---|---|
| `/app` | `~/Library/Containers/<bundle-id>/Data/.app-bundle/` |
| `/data` | `~/Library/Containers/<bundle-id>/Data/` |
| `/cache` | `~/Library/Containers/<bundle-id>/Data/Library/Caches/` |
| `/tmp` | `/private/tmp/` (sandboxed; auto-evicted) |
| `/shared/<channel>` | `~/Library/Group Containers/<group-id>/` (if both apps share group) |

### 5.2 Tessera on macOS

Tessera (Atrium CAS-FS) is not the macOS filesystem.
Two paths:

- **Phase 1 (MVP):** Tessera userspace tooling stores
  blobs in a directory tree under
  `~/Library/Application Support/atrium/tessera/`.
  Lookup is by content hash via a small SQLite index.
  No kernel-level dedup; APFS clones approximate the
  property weakly.
- **Phase 2:** Tessera as a FUSE-via-macOS-FSKit
  filesystem, exposing the same CAS semantics. Same
  Tessera codebase, different mount target.

Apps see the same content-addressed API regardless of
which Phase backs them. The difference is invisible to
manifests and app code.

### 5.3 Powerbox-granted files

When the user picks a file via Scrinium (the picker),
the macOS implementation returns a **security-scoped
bookmark** in addition to the fd. This bookmark lets
the app re-open the same file across launches (with
user re-consent the first time per session).

This maps cleanly to Insula's powerbox capability shape
— the bookmark *is* the durable capability handle.

### 5.4 Workspace-shaped capabilities for Artifex

Artifex's workspace (a directory tree granted by
Scrinium) becomes a security-scoped bookmark to a
folder. macOS sandbox extension lets the bookmarked
subtree be readable + writable for Artifex; outside
the subtree remains denied.

## 6. Identity and Vestibulum bridge

### 6.1 Per-service keypairs in macOS Keychain

Vestibulum's per-service-keypair model (`insula.md`
§13.3) maps to macOS Keychain items:

- Service name = (persona id, service id) pair.
- Account name = key-id.
- Generic password = the private key bytes (with
  `kSecAttrAccessControl` requiring user presence /
  biometric).

The Vestibulum-macos-bridge is the thin shim that
implements the Vestibulum Aqueduct API on top of
SecItem-add/SecItemCopyMatching.

### 6.2 LocalAuthentication for unlock

Touch ID / passcode / password prompts use
LocalAuthentication.framework. This is how sign-in
flows surface authentication challenges — Vestibulum's
"prove user presence" hook calls LA on macOS.

### 6.3 What Vestibulum does NOT use on macOS

- iCloud Keychain. Sync (if enabled) goes through
  Atrium's sync subsystem, not iCloud. (Mixing the
  two would create confusing trust-model behavior.)
- macOS Login Keychain. Per-service keypairs live in
  a Vestibulum-dedicated keychain that the host
  adapter creates, distinct from the system Login
  keychain.

## 7. Compute limits

App Sandbox doesn't have rctl-equivalent CPU/RSS
limits at the FreeBSD-strict level. The host adapter
approximates:

| Insula limit | macOS implementation |
|---|---|
| `cpu = "100ms/s"` | NSProcessInfo QoS class + system-managed AppNap; coarse but effective on macOS |
| `rss = "256MB"` | `setrlimit(RLIMIT_AS, …)` + NSProcessInfo `processInfo.physicalMemory`-aware throttling; OOM kill at hard cap |
| `wall = "unbounded"` | no enforcement; OS reaps inactive processes per usual macOS policy |
| `max-runtime` for triggered-bg | `dispatch_after` timer + SIGKILL at expiry |

Effective protection is coarser than Atrium's rctl,
but sufficient — apps still cannot consume unbounded
resources, and the user has macOS-level visibility
into per-app CPU/RAM via Activity Monitor.

## 8. Aqueduct on macOS

### 8.1 Transport

Aqueduct on macOS uses two transports under the hood:

- **Unix domain sockets** for local-only same-machine
  IPC (default).
- **Mach ports** for cases where Mach-port semantics
  are required (sandbox-cross interactions; macOS
  XPC compatibility).

Apps see the same Aqueduct API; the transport choice
is internal.

### 8.2 SCM_CREDS equivalent

FreeBSD `SCM_CREDS` provides kernel-attested peer
identity. macOS equivalent: `LOCAL_PEERCRED` for unix
sockets, audit-token-from-Mach-port for Mach. Both
suffice for Aqueduct's "who sent this message"
guarantee.

### 8.3 Network-traversing Aqueduct

For `insula.md` §20 (distributed apps + remote
rendering), Aqueduct over network uses TLS over TCP/
QUIC, with Vestibulum-attested device identities. Same
on every host; no macOS-specific work beyond linking
to a TLS library (likely BoringSSL).

## 9. Pergola on macOS

Already partially done in existing Atrium bring-up:
- Fresco runs on Metal via MoltenVK (per memory,
  validated 2026-05-10).
- Pergola sits on top with the same widget API.

What this spec adds: the Pergola macOS backend should
emit *macOS-native interaction conventions* where
applicable — Cmd-key bindings, macOS-style modal
sheets, native menu bar integration via NSMenu — even
though the widget code is the same. These are
*platform conventions* layered on top of Pergola's
identical-everywhere widget API.

## 10. Bundle and install

### 10.1 The .insula bundle format on macOS

An Insula bundle has the same content on every host
(signed manifest + ELF/Mach-O binaries + assets). On
macOS, the bundle is wrapped in an `.app` directory
structure for compatibility with macOS install
expectations:

```
Artifex.app/
├── Contents/
│   ├── Info.plist                    ; macOS-required
│   ├── MacOS/
│   │   └── Artifex                   ; Mach-O binary
│   ├── Resources/
│   │   └── ...
│   ├── Insula/
│   │   ├── manifest.cbor             ; the Insula manifest
│   │   ├── signature
│   │   └── ...
│   └── _CodeSignature/               ; macOS code-signing data
```

The `Contents/Insula/` subtree is the actual Insula
bundle; the surrounding `.app` structure is macOS-
visible packaging.

### 10.2 Distribution paths

Multiple distribution channels:
- **Opifex via the platform registry** — the
  preferred path; Opifex handles install + manifest
  registration + sandbox profile generation. The
  `.app` is created on the user's machine, not
  shipped pre-wrapped.
- **Direct `.app` download** — works for testing;
  user double-clicks; the `.insula-host` recognizes
  Insula apps and registers them.
- **Mac App Store** — possible but requires Apple
  review and conformance to App Store rules. Out of
  scope for v0; deferred.

### 10.3 Code signing

All Insula apps on macOS must be signed:
- **Atrium publisher signature** — required by Insula
  for content-addressed verification.
- **macOS Developer ID signature** — required by
  macOS for Gatekeeper. For .insula-host to install
  apps without Gatekeeper friction, it should be
  signed by an Atrium-team Developer ID and notarized.

The two signatures coexist; both verify.

### 10.4 Notarization

`.insula-host` and bundled platform apps go through
Apple's notarization. Third-party Insula apps signed
by their publishers need their own notarization for
smooth macOS install — or the user accepts an
"unidentified developer" warning.

In the long run, Atrium-team could submit a per-
publisher delegated-signing arrangement with Apple,
but for v0 each publisher handles their own
notarization.

## 11. Differences from Atrium-native and v0 limits

### 11.1 What is weaker

- **Per-jail GPU contexts** — macOS's GPU isolation is
  driver-managed; Atrium GPU ABI's per-jail context
  guarantee is stronger.
- **Filesystem dedup** — Tessera-on-macOS Phase 1 has
  no kernel dedup; APFS clones are weak approximation.
- **rctl-equivalent CPU/RSS** — coarser on macOS than
  on Atrium FreeBSD.
- **Kernel-enforced jail vs. App Sandbox** — App
  Sandbox is widely-deployed but has had occasional
  bypasses; Atrium's FreeBSD jail + capability model
  is architecturally stricter.

### 11.2 What is the same

- All userspace Insula services (same code).
- All capability-shape and powerbox semantics.
- All AX, Limen, Tabellarius, Loculus, Concursus,
  Nomenclator behavior visible to apps.
- Bundle format, manifest schema, signing model.

### 11.3 What is stronger on macOS (rare)

- **TCC integration for camera / microphone / location
  / contacts.** macOS already has a permission-prompt
  system for these; Insula on macOS leans on it rather
  than reinventing.
- **Notarization-class chain of trust.** Apple's
  trust infrastructure provides a strong "this binary
  was reviewed" signal; Atrium's chain catches up
  over time.

## 12. Bring-up phases (within Phase 1 of `insula.md` §0.7.3)

### 12.1 Phase 1A — minimum host

- `.insula-host` shell as a single signed macOS app.
- portcullis-shim implementing app-launch with sandbox
  profile generation.
- Aqueduct on Unix sockets + LOCAL_PEERCRED.
- atrium-netd-macos basic broker.
- Vestibulum-macos-bridge for Keychain.

Goal: an Insula app — Artifex's MVP — launches,
sandboxed, talks to Aqueduct services, can fetch one
allowlisted host.

### 12.2 Phase 1B — Insula service catalogue

- limend, tabellariusd, loculusd, concursusd,
  nomenclatord running as launchd agents.
- atrium-ax-d publishing the composed tree.
- Scrinium picker (powerbox) with security-scoped
  bookmark output.

Goal: full Insula contract surface available to apps
on macOS.

### 12.3 Phase 1C — production polish

- Tessera-FSKit filesystem (Phase 2 of §5.2).
- Notarization pipeline integration.
- Distribution UX (Opifex installs on macOS without
  Gatekeeper friction).
- macOS-conventional UI affordances throughout
  Pergola.

Goal: Insula on macOS is a serious development +
production target, not just a bring-up scaffold.

## 13. Open questions

- **App-Store distribution.** Is Insula apps via Mac
  App Store a goal or a non-goal? Pros: smooth user
  install + Apple's notarization-by-default. Cons:
  Apple review, Apple commercial terms, Apple veto
  of capabilities. Likely v2+.
- **Per-publisher delegated notarization.** Could
  Atrium-team negotiate with Apple for a
  delegated-signing arrangement that lets publisher
  signatures suffice for Gatekeeper? Out of scope for
  v0; valuable longer-term.
- **Tessera-FSKit vs. directory blob store.** Phase 1A
  uses directory + SQLite; Phase 2 wants FSKit. FSKit
  is recent and the API surface is still maturing;
  evaluation pending.
- **GPU isolation parity.** Atrium GPU ABI provides
  per-jail contexts; macOS Metal doesn't natively.
  Acceptable for v0 (single-user device); revisit
  if multi-tenant Atrium-on-macOS use case emerges.
- **Linux host adapter sibling.** This spec is
  macOS-specific; the Linux adapter will be a sibling
  spec (insula-host-linux.md) when Phase 2 of the
  Insula bring-up begins. Many techniques transfer
  (broker shape, manifest mapping); the primitives
  differ (Landlock + seccomp + namespaces vs App
  Sandbox).

## 14. References

- `docs/spec/insula.md` — parent spec; §0.7 is the
  portability strategy, §4 is the sandbox section
  whose macOS implementation lives here.
- `docs/spec/limen.md`, `tabellarius.md`, `loculus.md`,
  `concursus.md`, `nomenclator.md`, `atrium-ax.md` —
  the services this host adapter hosts.
- `docs/spec/aqueduct.md` — substrate; this spec
  describes the macOS transport.
- `docs/spec/pergola.md` — UI toolkit; Pergola-on-
  Metal is the macOS rendering backend.
- `docs/spec/tessera-fs.md` — the on-macOS storage
  story is described here in §5.
- `docs/NAMING.md` — naming reference.
- Apple developer documentation:
  - App Sandbox Design Guide
  - LaunchAgents
  - Network.framework
  - Security.framework (Keychain Services)
  - FSKit (Phase 2)
