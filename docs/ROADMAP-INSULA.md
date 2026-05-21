# Roadmap — Insula

Sibling roadmap to [`docs/ROADMAP.md`](ROADMAP.md). That document
covers the Atrium OS phases (D0–D7); this one covers the
Insula app-platform layer.

Insula runs on macOS, Linux, Windows, and Atrium (per
`insula.md` §0.7). Its bring-up is independent of Atrium's
OS work — the macOS-first plan deliberately decouples them
so adoption can accumulate without waiting on Atrium D-phase
completion.

For each milestone:
- **Deliverable** — visible outcome.
- **Inputs** — which sibling-spec phases land here.
- **Dependencies** — what must already exist.
- **Risks** — what could go wrong.
- **Estimate** — focused-engineer-months.

The four overall bring-up phases (per `insula.md` §0.7.3):

| Phase | Target | Goal |
|---|---|---|
| 1 | macOS | reference SDK + Artifex + service catalogue |
| 2 | Linux | second host adapter; validate portability |
| 3 | Windows | third host adapter; cover the desktop tri-fecta |
| 4 | Atrium | first-class native experience |

This document drills into Phase 1 (the critical path) in
detail; Phases 2–4 are sketched.

## 0. Cross-spec dependency graph

The eight Insula sibling specs each have their own phase
plans (A, B, C, … per spec). The dependencies between
them, simplified:

```
insula-host-macos Phase 1A   ← trunk; everything depends on this
            │
            ├── libatrium.so + manifest format
            │           │
            │           ├── Aqueduct on macOS
            │           │           │
            │           │           ├── Limen Phase A
            │           │           │           │
            │           │           │           ├── Artifex Phase A (editor + Stoa)
            │           │           │           │           │
            │           │           │           │           ├── Artifex Phase B (LSP + DAP)
            │           │           │           │           │
            │           │           │           │           └── Artifex Phase C (extension API)
            │           │           │           │                      ↑ Limen Phase B (full role catalogue)
            │           │           │           │
            │           │           │           ├── atrium-ax Phase A (per-app)
            │           │           │           │           │
            │           │           │           │           └── atrium-ax Phase B (composition) ← Limen Phase B
            │           │           │           │
            │           │           │           └── Nomenclator Phase A
            │           │           │                       │
            │           │           │                       ├── Loculus Phase A
            │           │           │                       └── Tabellarius Phase A
            │           │           │                                   │
            │           │           │                                   └── Concursus Phase A
            │           │           │
            │           │           └── Vestibulum-macos-bridge
            │           │                       │
            │           │                       └── Loculus / Tabellarius / Concursus depend
            │           │
            │           └── atrium-netd-macos (network broker)
            │
            └── Pergola macOS backend (existing work; production-hardened in parallel)
```

Key chokepoints:
- **`insula-host-macos` Phase 1A is the trunk.** Nothing
  else works until this lands.
- **Limen Phase A unblocks Artifex's extension model.**
- **Aqueduct on macOS unblocks every Insula service.**

Parallelism opportunities:
- After M1A, several services can be developed in parallel
  by independent teams.
- Artifex Phase A only needs M1B (Stoa + a working
  editor surface); it can advance in parallel with
  service catalogue work.
- Pergola macOS hardening (existing work) can advance
  concurrently with all the above.

## 1. Phase 1 (macOS) — the critical path

### M1A — Foundation

**Deliverable:** A signed, sandboxed Insula app launches
on macOS, prints to stderr, exits cleanly. No real
services yet. The host shell + plumbing works.

**Inputs:**
- `insula-host-macos.md` Phase 1A.

**Concrete sub-tasks:**
1. `.insula-host` macOS app skeleton (Swift / Objective-C +
   Rust core).
2. `portcullis-shim`: manifest-to-SBPL profile generation;
   manifest-to-entitlements XML; signed app launch via
   `posix_spawn` + sandbox flags.
3. `libatrium.so` first-pass C ABI stubs: `atrium_init`,
   exit, logging.
4. Manifest CBOR parser + signature verification.
5. Trivial sample app: `hello-insula` — prints "Insula on
   macOS" to stderr, exits.
6. End-to-end CI on a real macOS runner.

**Dependencies:** existing macOS toolchain (clang, codesign,
notarytool); existing Atrium repo's Rust build setup;
NAMING.md naming locks.

**Risks:**
- macOS App Sandbox has quirks that don't surface until
  real apps run; iterating on profile correctness can
  consume weeks.
- Code signing + notarization pipeline is bureaucratically
  finicky; expect setup friction.

**Estimate:** 3 months focused.

### M1B — Service catalogue MVP

**Deliverable:** A sample app launches, opens a Pergola
window, fetches data from one allowlisted host, signs in
to a service (records key in keychain), and shuts down.
The full minimum service surface works.

**Inputs:**
- Pergola macOS backend production hardening (existing
  work).
- Aqueduct on macOS: Unix-socket transport,
  `LOCAL_PEERCRED` for peer identity, Mach-port fallback
  for sandbox-cross cases.
- `atrium-netd-macos` MVP: hostname-allowlist broker
  using `Network.framework`.
- `Vestibulum-macos-bridge` MVP: SecItem-add /
  CopyMatching wrapped behind the Vestibulum API; per-
  service keypair mint via LocalAuthentication.
- `Nomenclator` Phase A: DNS TXT lookup, ed25519
  manifest signature, in-memory cache, `atrium-doc://`
  resolution end-to-end.
- `Stoa-shim` macOS: minimal pty-shaped terminal session
  attachable to apps.

**Concrete sub-tasks:**
1. Aqueduct macOS daemon + client library.
2. atrium-netd-macos broker: enforce host allowlist,
   resolve DNS, deliver fd to app.
3. Vestibulum bridge: per-service keypair generation,
   signing-challenge API.
4. Nomenclator daemon: DNS TXT, fetch + verify, cache.
5. Stoa-macos shim: spawn shell in workspace fd.
6. Sample app `weather`: connects to mock weather API,
   signs in, persists user identity.

**Dependencies:** M1A.

**Risks:**
- Pergola on macOS may need real production-hardening
  beyond the existing Fresco-on-Metal first-light work.
  Could double the M1B estimate if Pergola surface area
  is more than expected.
- `LOCAL_PEERCRED` semantics are well-documented but the
  Mach-port path for sandbox-cross identity attestation
  is messier; might need iteration.

**Estimate:** 4 months focused.

### M1C — Artifex MVP

**Deliverable:** A developer opens Artifex, opens a Rust
project, edits code with syntax highlighting and rust-
analyzer completions, uses a Stoa terminal, makes a git
commit. The reference IDE is showable.

**Inputs:**
- `artifex.md` Phase A.
- Limen Phase A (just enough for Stoa-as-embed).
- Tree-sitter integration in Pergola text widgets.

**Concrete sub-tasks:**
1. Artifex's editor surface (Pergola text widgets, rope
   buffer, multi-cursor, tree-sitter highlighting).
2. Workspace management via Scrinium-granted folder
   (security-scoped bookmark).
3. rust-analyzer LSP integration (JSON-RPC framed in
   Aqueduct).
4. Stoa terminal embed (Limen `terminal` role minimal).
5. Native git status decorations via `gix` (Rust libgit2-
   equivalent).
6. `.artifex/` workspace state.
7. Open-file UX, save, close, find/replace (the table-
   stakes of a working editor).

**Dependencies:** M1B, Limen Phase A (Stoa role).

**Risks:**
- Pergola text widget performance on macOS is the
  single biggest risk; Fresco+MoltenVK has worked at
  the first-light level but production-quality text
  rendering (subpixel AA, ligatures, complex scripts)
  is significant work.
- Rope-buffer + tree-sitter integration is industry-
  standard but needs care with mmap-of-huge-files
  semantics.

**Estimate:** 6 months focused. This is the showcase
deliverable.

### M1D — Service catalogue completion

**Deliverable:** Loculus, Tabellarius, atrium-ax,
Concursus all running. A sample app can autofill an
address, receive a push notification, be screen-reader-
accessible, and connect to a peer on the same LAN.

**Inputs (each is a Phase A from its sibling spec):**
- Limen Phase B (full role catalogue including
  `autofill`, `picker`, `share-target`, `payment`, `map`).
- Loculus Phase A (addresses + profiles).
- Tabellarius Phase A (single-relay MVP).
- Concursus Phase A (same-LAN direct + file-share).
- atrium-ax Phase A (single-app AX) + Phase B
  (composition via Limen).

**Concrete sub-tasks:** see each sibling spec's Phase A
section.

**Dependencies:** M1B (service catalogue MVP); some
parallelism with M1C.

**Risks:**
- Tabellarius's relay operations: who runs the default
  relay during bring-up? Either Atrium-team self-hosts
  or partners with an existing push provider. Decision
  pending.
- atrium-ax composition across jail boundaries on macOS
  is novel; might surface unexpected complexity.

**Estimate:** 4 months focused, parallel with M1C.

### M1E — Artifex Phase B and production polish

**Deliverable:** Artifex is a serviceable IDE for
working Insula developers (the §11.2 phase-B target).
Insula on macOS is in a "developer beta" state.

**Inputs:**
- Artifex Phase B (multi-LSP, DAP debugging, code
  intelligence, multi-cursor commands, search).
- `insula-host-macos` Phase 1C (Tessera-FSKit
  filesystem, notarization polish).
- atrium-ax Phase C (production polish).
- Limen Phase C (A11y bridge integration).

**Concrete sub-tasks:**
1. Artifex multi-LSP server orchestration.
2. DAP integration with `lldb`-shaped adapter; `debug`
   capability surfaced via macOS.
3. Code intelligence UX (go-to-def, find-refs, rename,
   completions).
4. Search across workspace (ripgrep worker).
5. Tessera-FSKit filesystem (decision to ship as FSKit
   vs. directory store is in `insula-host-macos.md`
   §13).
6. Notarization pipeline integration with Opifex install
   flow.

**Dependencies:** M1C + M1D.

**Risks:**
- DAP + macOS `task_for_pid` entitlement: getting the
  debug capability requires special signing
  arrangements with Apple, or shipping as a
  developer-tools entitlement that requires the user to
  re-trust.
- FSKit is recent and the API surface is still
  maturing; fallback to the directory-blob-store path
  is the safe default.

**Estimate:** 6 months focused.

### Phase 1 total

| Milestone | Months | Cumulative |
|---|---|---|
| M1A | 3 | 3 |
| M1B | 4 | 7 |
| M1C | 6 (concurrent w/ M1D) | 11 |
| M1D | 4 (concurrent w/ M1C) | 11 |
| M1E | 6 | 17 |

**Total Phase 1: ~17 focused-engineer-months on the
critical path.** With three concurrent engineers, real
calendar time is ~6 months for the critical milestones
+ overhead; honest range is 12–18 months calendar to
the M1E deliverable.

## 2. Phase 2 (Linux)

**Deliverable:** A new host adapter; everything that
worked on macOS now works on a Linux desktop.

**Scope:**
- New spec: `docs/spec/insula-host-linux.md`.
- Sandbox primitive: Landlock + seccomp +
  user/mount/network namespaces (bubblewrap-shape).
- Service launch: systemd user units, or a standalone
  supervisor.
- Networking: netns + nftables + the same broker shape.
- Identity: Secret Service / libsecret bridge.
- Filesystem: same Tessera userspace path (no kernel
  module yet — Linux Tessera kmod is its own deferred
  project).

**Estimate:** 3–4 months focused.

**Why faster than macOS Phase 1:** the userspace services
(Limen, Tabellarius, Loculus, Concursus, Nomenclator,
atrium-ax) are already written and known to work. Only
the host adapter is new. The macOS bring-up did the hard
work of validating the host-adapter abstraction.

## 3. Phase 3 (Windows)

**Deliverable:** Insula on Windows desktop; AppContainer
+ Job Objects as the sandbox.

**Scope:**
- `docs/spec/insula-host-windows.md`.
- Sandbox: AppContainer + Job Objects.
- Service launch: Service Control Manager or a
  standalone supervisor.
- Networking: WFP + broker.
- Identity: Credential Manager bridge.

**Estimate:** 4–5 months focused.

**Why slightly slower than Linux:** Windows is the most
different of the three from FreeBSD/Atrium primitive-
wise; AppContainer + WFP are less Unix-shaped than
Landlock + namespaces.

## 4. Phase 4 (Atrium)

**Deliverable:** Insula running natively on Atrium with
the *purpose-built* benefits — kernel-enforced
Portcullis jails, Tessera CAS-FS native, Atrium GPU ABI
acceleration paths, the works.

**Scope:**
- Already covered by `insula.md` §0.7 + the existing
  Atrium D-phase roadmap.
- Major pieces inherited from Atrium D-phases (D0–D5)
  — kernel modules, native GPU, Tessera FS, jail model.
- Insula-specific work: the host-adapter abstraction's
  Atrium implementation. Mostly mechanical given the
  abstraction was already designed.

**Dependencies:** Atrium D-phases through D5 at minimum.

**Estimate:** ~12 months focused, but spread across the
Atrium D-phase timeline (which has its own multi-year
schedule per `ROADMAP.md`).

## 5. Risks across phases

### 5.1 Pergola maturity on macOS

The single biggest risk. Fresco + MoltenVK first-light
exists but production-quality Pergola — subpixel-
correct text, smooth animation, robust input handling,
real i18n — is multi-month additional work that gates
M1B/M1C.

**Mitigation:** treat Pergola macOS hardening as a
parallel track from M1A onward, not a serial
dependency on a single milestone.

### 5.2 Relay-operator question

Tabellarius needs a default push relay. Concursus
benefits from one. Who runs it during bring-up?

**Mitigation:** ship a reference relay implementation
that anyone can self-host; partner with an existing
relay during early days; defer the "official Atrium
relay" question until Phase 1D.

### 5.3 macOS App Sandbox edge cases

macOS sandbox is well-documented but has historically
had bypass-class issues. None catastrophic for Insula
since Insula doesn't claim FreeBSD-jail-strict
isolation on macOS, but enough that the host adapter
needs careful auditing.

**Mitigation:** explicit list of "known sandbox bypass
classes; what they let an app do; documented
acceptable-risk posture" in `insula-host-macos.md`
(open question §13).

### 5.4 Apple platform politics

macOS notarization, Gatekeeper, App Store rules are
all under Apple's discretion. Insula doesn't depend on
the App Store but does depend on Developer ID
notarization for smooth user install.

**Mitigation:** the bundle format is designed to *also*
work as a directly-downloaded `.app` without
notarization (user accepts an "unidentified developer"
prompt). Notarization is a UX nicety, not a
correctness requirement.

### 5.5 IR distribution complexity

`insula.md` §3.3's WASM-as-IR + Cranelift AOT path is
elegant but engineering-heavy. Native ELF/Mach-O
distribution is the default and works without this
machinery.

**Mitigation:** defer IR distribution to Phase 1E or
later. Phase 1 ships native-only.

## 6. Parallelism strategy

Given the dependency graph (§0), here's how a small team
parallelizes:

**Team of 3 engineers (the realistic small case):**
- Engineer A: host adapter critical path (M1A → M1B →
  M1E host pieces).
- Engineer B: Pergola macOS hardening (parallel from
  M1A onward).
- Engineer C: Artifex (joins at M1B; through M1C/M1E).

After M1B, parts can fan out:
- Engineer A focuses on M1D services.
- Engineer C continues Artifex.
- Engineer B continues Pergola + atrium-ax + Limen
  composition.

**Team of 5–8 engineers (the well-funded case):**
Each major sibling spec (Limen, Tabellarius, Loculus,
Concursus, Nomenclator, atrium-ax) gets a primary
owner; insula-host-macos gets one; Artifex gets one;
Pergola macOS hardening gets one. Most work parallelizes
after M1B.

## 7. Strategic checkpoints

Three moments where the project should pause and
evaluate:

### 7.1 After M1A — "does the abstraction work?"

The host-adapter concept is the key architectural bet.
If M1A is bloated or hacky, the abstraction is wrong
and we should rethink before proceeding to M1B.

### 7.2 After M1C — "is Artifex compelling?"

This is the public bring-up demo. If Artifex is *not*
visibly better than VS Code on the §10.9.5 perf axes,
the no-Electron pitch is hollow. Course-correct or
restage.

### 7.3 After M1E — "is the platform real?"

By M1E we have a developer beta. Real developers using
it for real work is the test of whether Insula's
contract is right or wrong. The "developer beta period"
should accumulate ~6 months of feedback before
committing to Phase 2 (Linux).

## 8. What this roadmap does not address

- **Specific funding / staffing model.** Estimates here
  are focused-engineer-months; turning that into a
  funded project is separate.
- **Atrium D-phase scheduling.** Already in
  `ROADMAP.md`. Phase 4 inherits whatever Atrium
  D-phase status exists at that point.
- **Insula API freeze policy.** The platform ABI
  (libatrium.so, manifest schema, role catalogue)
  needs explicit versioning + freeze discipline. Worth
  its own short doc — `docs/INSULA-VERSIONING.md`
  perhaps — before M1C ships to external developers.
- **Ecosystem / marketing / community.** Out of scope
  for this engineering roadmap.

## 9. References

- `docs/ROADMAP.md` — Atrium OS phases (parent roadmap
  for the OS layer).
- `docs/spec/insula.md` — parent spec; §0.7 is the
  cross-OS bring-up strategy.
- All eight Insula sibling specs (each has its own
  phase plan that this roadmap composes).
- `docs/NAMING.md` — naming reference.
