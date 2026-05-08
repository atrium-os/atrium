# `atrium-pkg` install UX — install card, progress, capability prompts

**Status:** spec, 2026-05-08
**Owner:** D2.5 packaging track + D3 forum (dock)
**Companion to:**
- `docs/spec/atrium-pkg.md` — package format, install path
- `docs/spec/atrium-pkg-registry.md` — registry, signing, attestation
- `docs/spec/atrium-jail.md` — snapshot/import (shares this UX)
- `docs/spec/portcullis.md` — capability schema
- `docs/spec/pergola.md` — UI toolkit

How package install *feels*. The technical install pipeline
(fetch → verify → build → materialize → register) is documented
elsewhere; this spec covers the user-facing surface. The
constraint is that **source-from-scratch installs are slow** —
a serious app may take 5–30 minutes to build — and the UX has
to make that bearable, honest, and not feel broken.

The audience is a discerning user who understands they are on a
build-from-source platform (`docs/spec/atrium-pkg-registry.md`
§10). They will tolerate slow installs if the experience is
informative and non-blocking. They will not tolerate "spinner
that hasn't moved in 4 minutes" or "popup that blocks me from
doing anything else."

## 1. Principle

> **Install is a background activity surfaced as an "install
> card" that the user can dismiss to a tray and ignore. The
> card shows real-time, structured progress with honest time
> estimates. The launcher icon for the app appears immediately
> in a "preparing" state and fills in as the build progresses.
> The user's other work is never blocked.**

What this is *not*:

- Not a modal install dialog. Modal install dialogs are how
  Windows installers look; they signal "your computer is busy
  for the next 10 minutes." Atrium installs are background.
- Not a fake-progress bar. Source-build progress is real and
  per-crate; we surface what `cargo` actually emits.
- Not "installing… please wait" with no detail. The user sees
  what step we're on, how big it is, and how long it's likely
  to take.

## 2. Install lifecycle and states

An install moves through these states. Each is observable in
the install card:

| State | Description | Typical duration |
|---|---|---|
| `queued` | Install requested, waiting for prior installs to release a build slot | 0s–minutes (depends on queue) |
| `fetching-manifest` | Downloading + verifying registry manifest | <1s |
| `verifying-signature` | Querying Sigstore Rekor, verifying publisher identity | 1–2s |
| `prompting-capabilities` | Showing user capability declarations, awaiting consent | user-bounded |
| `fetching-source` | Downloading source tarball or binary blob | 5s–60s (size-dependent) |
| `verifying-blob` | Hash check against manifest's declared sha256 | 1–5s |
| `resolving-deps` | `cargo` (or other) resolving dependency graph | 5–20s |
| `building` | Compiling — *the long phase* for source installs | 30s–30min |
| `materializing` | Ingesting build output into Tessera CAS, allocating volumes, registering jail | 1–5s |
| `ready` | Install complete, app available in launcher | terminal state |
| `failed` | One of the above steps errored | terminal state, with diagnostic |
| `cancelled` | User cancelled mid-install | terminal state |

The `building` state is itself sub-divided for source installs:
the install card shows current crate name and N-of-M progress.
For binary installs, `building` is skipped entirely — the
critical-path is `fetching-source` (which is really
`fetching-binary`).

## 3. Install card

### Visual shape

The install card is a Pergola-rendered surface (default
animation = spring per Pergola's design decisions) anchored to
the **notification shelf** (D3 forum) — top-right of the
display, above the dock. It is non-modal: the user can keep
working, switch focus, even close the launcher; the card
persists.

Card structure:

```
┌─────────────────────────────────────────────┐
│ [icon]  Firefox 125.0                  [×] │
│                                             │
│ Building (43 / 312 crates)                  │
│ ▓▓▓▓░░░░░░░░░░░░░░░░░░░░░░░░░░  14%        │
│                                             │
│ Compiling tokio v1.42                       │
│ ~8 minutes remaining                        │
│                                             │
│ [Pause]  [Cancel]  [Hide to tray]          │
└─────────────────────────────────────────────┘
```

Per-state content varies:

- `prompting-capabilities`: full capability list, per-cap
  human-readable explanation, [Approve] / [Reject] buttons
  (see §5).
- `failed`: error summary, [Show details] expandable, [Retry]
  / [Report to publisher] / [Dismiss] buttons.
- `ready`: brief "Firefox installed" with [Launch] /
  [Dismiss] buttons; auto-fades after 10s if not interacted
  with.

### Tray representation

Hidden cards live in the notification shelf as a small icon
with a circular progress indicator. Hovering shows a tooltip
("Firefox: building, 43/312 crates"). Clicking restores the
full card.

Multiple concurrent installs each get a card; the shelf shows
a stack with a count badge.

### Launcher icon "ghost" state

The moment install starts, the app's icon appears in the
launcher in a **ghosted state** (Pergola: same icon at 50%
opacity, with a thin progress ring around it). Clicking the
ghosted icon opens the corresponding install card. When
install finishes, the icon transitions to fully opaque with a
spring animation — a small visible reward for the wait.

This serves two purposes:
1. The user has a tangible signal "yes, your app is on its
   way" the moment they click install — no wondering "did the
   click register?"
2. They can find their pending install later by looking at
   the launcher, even if they dismissed the card.

## 4. Progress reporting (the long phase)

For source builds, the build phase is by far the longest. We
surface real progress, not theatrics:

- **Per-crate progress.** Cargo emits structured JSON build
  output (`cargo build --message-format=json`); we parse it
  and update the card on every `compiler-artifact` event.
  Format: "Compiling \<crate\> v\<version\>". Counter:
  N-of-M, where M is the total in the dep graph (known from
  the resolve phase).
- **Long pauses are explained.** A single crate that takes
  >60 seconds (typically the leaf crate's link step) gets a
  hint: "Linking firefox — this is the final step, almost
  done." This is the most common point users think the
  install is hung.
- **Time estimate** based on cumulative crate compilation
  rate so far, with a confidence band as text ("~8 minutes
  remaining" or "10–15 minutes remaining"). Updated every few
  seconds.
- **No fake spinners.** If the build is genuinely making no
  progress (nothing emitted from cargo for >120s), surface
  that explicitly: "Build appears stalled — show details?"
  rather than letting the spinner spin.

For binary installs, progress is download-byte progress,
which is straightforward.

## 5. Capability prompt

Before any download begins (after manifest fetch + signature
verify), the install card shows a capability consent prompt.
This is the user's chance to refuse based on what the package
wants access to.

### Layout

```
┌─────────────────────────────────────────────┐
│ Install Firefox 125.0?                      │
│                                             │
│ Publisher: github.com/mozilla (verified)    │
│                                             │
│ This app requests:                          │
│  ✓ Internet access (any host)               │
│  ✓ Graphics device                          │
│  ✓ Read your Documents folder               │
│  ✓ Read/write your Downloads folder         │
│  ✓ Microphone (when you grant per-site)     │
│                                             │
│ [Approve]  [Customize]  [Cancel]            │
└─────────────────────────────────────────────┘
```

### Per-capability explanation

Each declared capability surfaces a human-readable explanation,
not the raw schema name. Mapping is centralized in
`portcullis.md` capability schema; UI just consumes it.

```toml
# Excerpt from portcullis capability registry
[capabilities."network.outbound"]
display = "Internet access"
risk = "medium"
default_explanation = "This app can connect to internet servers."

[capabilities."device.gpu"]
display = "Graphics device"
risk = "low"
default_explanation = "This app can render with the GPU. Standard for most apps."

[capabilities."filesystem.user-documents"]
display = "Read your Documents folder"
risk = "high"
default_explanation = "This app can read every file in your Documents folder."
```

### Risk levels and visual treatment

- **low**: gray check, no special treatment (`device.gpu`,
  `device.audio`, etc.).
- **medium**: blue accent (`network.outbound`,
  `filesystem.user-downloads` rw).
- **high**: amber accent + "Read your X" prefix
  (`filesystem.user-documents` ro/rw,
  `filesystem.user-home` any).
- **critical**: red accent + warning icon + extra confirmation
  ("Are you sure?") on approval (`network.host`,
  `device.kvm`, `filesystem.system`, etc.).

### Customize panel

Clicking [Customize] expands a per-capability toggle list.
Disabling a capability that the package declares as `required`
shows a warning ("App may not function correctly"); disabling
one declared as `optional` is silent. Optional caps are
re-prompted at first use (lazy capability grant).

### Capability diff on upgrade

When upgrading from `firefox 124.0` (already installed) to
`125.0`, the prompt shows **only** the diff:

```
┌─────────────────────────────────────────────┐
│ Update Firefox 124.0 → 125.0?               │
│                                             │
│ Publisher: github.com/mozilla (verified)    │
│                                             │
│ New capability requests:                    │
│  ⚠ Bluetooth device access                  │
│                                             │
│ Removed:                                    │
│  · (none)                                   │
│                                             │
│ Existing capabilities are unchanged.        │
│                                             │
│ [Approve update]  [Skip update]  [Cancel]   │
└─────────────────────────────────────────────┘
```

A new capability request on an upgrade is a strong signal —
the user might want to investigate why version 125 suddenly
needs Bluetooth. The amber-on-new visual draws attention
without blocking.

## 6. Optimistic launch (post-v1, opt-in per publisher)

A publisher *may* ship a tiny "splash bundle" alongside their
main binary — a few KB containing the app's launch splash
screen image + brand identity. If present:

- The moment user clicks Install, the splash bundle downloads
  immediately (fast, tiny).
- The user can launch the app *before the build completes*.
  The launch shows the publisher's splash screen with a
  "preparing your first launch…" overlay.
- When the build finishes, the splash transitions to the
  real app with a spring animation.
- On second launch (post-install), the splash shows briefly as
  the real app starts, just like any other app.

This is opt-in, additive complexity for publishers who care
about first-impression UX. Default behavior for publishers who
don't ship a splash: the app icon stays ghosted in the
launcher and clicking it opens the install card.

Defer to v2; not load-bearing for the install path itself.

## 7. Concurrency and resource budgets

Multiple installs can be in-flight. The build subsystem manages:

- **Build slot pool.** Configurable, default
  `min(physical_cores, 4)` concurrent build jails. New installs
  enter `queued` state until a slot frees.
- **Per-build core budget.** Each build jail gets `cargo build
  --jobs=N` where N is `total_cores / active_builds`. Avoids
  thrash when 3 users start `firefox`/`chromium`/`thunderbird`
  installs simultaneously.
- **Download is independent of build slots.** Downloads happen
  in parallel with builds; manifest fetch + signature verify
  are cheap and never queue.

The user can explicitly pause low-priority installs from the
install card if they need full machine resources for foreground
work.

## 8. Cancellation, retry, error surface

### Cancel

User clicks [Cancel] from the install card. The system:

1. Sends SIGTERM to the build jail (if building); waits 5s;
   sends SIGKILL.
2. Cleans up partial build artifacts in the build jail's
   scratch space.
3. Leaves any already-fetched CAS blobs in place — they're
   content-addressed, harmless, and may speed up a retry. They
   become GC-eligible if no other manifest references them.
4. Updates the install card to `cancelled` state with [Retry]
   and [Dismiss] buttons.

### Retry

Same flow as a fresh install, but resumes from the latest
intact state — already-fetched blobs are reused, partial builds
are *not* (cargo's incremental state is unreliable across
restarts in a fresh jail).

### Error surfacing

When something fails, the user sees:

1. **One-line summary** in the card ("Build failed: error in
   crate `serde`").
2. **[Show details]** expandable, showing:
   - The actual rustc / cargo error (or download error, or
     verify error).
   - The phase it happened in.
   - Manifest provenance (who published this version).
3. **[Report to publisher]** button — pre-fills a GitHub issue
   on the publisher's repo with the error log + manifest
   reference. (Possible because the manifest has the publisher's
   GitHub identity from Sigstore.)
4. **[Retry]** for transient errors (network, mirror failures);
   greyed out for non-transient (build errors, signature
   failures).
5. **[Dismiss]** terminal — install state is `failed`, removed
   from launcher.

## 9. Uninstall UX

Mostly out of scope for this spec — uninstall is fast, doesn't
need a card. Brief mention:

- Uninstall is a confirmation dialog (one click, "Are you
  sure?") + a brief progress indicator that completes in
  seconds.
- Persistent volumes are NOT auto-deleted on uninstall (per
  `atrium-pkg.md` — operator must explicitly
  `atrium-volumes-cli destroy`). The uninstall confirmation
  shows what's being preserved with a "see also" pointer to
  the volumes management UI.

## 10. Telemetry-free instrumentation

The install card surfaces a "Show technical detail" affordance
that exposes:

- Manifest URL + hash
- Blob URL + hash + verified hash
- Sigstore Rekor entry URL
- Build environment fingerprint (toolchain version, target)
- Per-step timing breakdown
- For source builds: full cargo output log (saved to disk for
  the user to inspect)

None of this is sent anywhere. It's local-only diagnostic
information for the discerning user. If they want to share it
(bug report), they copy it themselves.

## 11. Layout and theming responsibility

The install card is a Pergola UI element. Its layout, animation,
and theming follow the Pergola design language:

- Spring animations as the default (Pergola decision 2026-05-04).
- Client-side layout (Pergola decision 2026-05-04).
- Accessibility surface: full keyboard navigation, screen-reader
  semantic labels, focus order tested.
- Dark mode / light mode tracked from system preference; honors
  `XDG_CURRENT_DESKTOP` settings or Atrium's equivalent.

The forum (D3 dock) owns the notification shelf and embeds the
install card; the package manager (D2.5) owns the *content* of
the card and the lifecycle state machine. Clean split: forum
draws, atrium-pkg drives.

## 12. Open questions / future work

- **First-install machine-wide hint.** On the very first
  install on a fresh Atrium machine, surface a one-time tip
  ("Atrium builds apps from source on your machine. This is
  slower the first time but means you can audit what you run.
  Most subsequent installs will reuse compiled dependencies."
  ← that last claim is currently false given §6 of
  atrium-pkg-registry.md, but might become true if we revisit
  the rlib-cache decision.)
- **Build-completion notifications.** When a long install
  finishes while the user is in another workspace / app, fire
  a system notification ("Firefox 125.0 ready — click to
  launch"). Tied to the notification shelf design.
- **Pause/resume across reboots.** A 25-minute compile
  interrupted by a reboot today restarts from scratch. Could
  serialize cargo's incremental state and resume — non-trivial
  because cargo's state isn't designed for that. Skip until
  there's a real complaint.
- **Bandwidth-aware fetch.** On metered connections, defer
  large blob fetches. Out of scope for v1; Atrium isn't
  positioned for mobile / metered links yet.
- **Optimistic launch (§6) implementation** — full design
  needed for the splash bundle format, the in-app
  "preparing…" overlay protocol, and the transition
  animation. Defer to v2.
