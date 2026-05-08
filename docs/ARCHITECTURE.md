# Atrium — architecture

> *Atrium: the central courtyard of a Roman house, surrounded by enclosed rooms with frescoed walls.*

**Atrium** is a FreeBSD-based desktop OS where the **security model**, the **storage model**, and the **graphics stack** are architecturally integrated rather than independently retrofitted. No Linux compatibility shims, no userspace GPU driver per process, no per-app filesystem duplication.

## Thesis

Concretely:

- **Security** — every app runs in a FreeBSD jail with explicit capability declarations. The jail launcher is **Portcullis**.
- **Storage cost** is bounded by content-addressed file dedup: 1 000 jailed apps store ~1 app's worth of unique bytes on disk. The CAS filesystem is **Tessera**.
- **Graphics** comes from **Fresco** — a retained-mode, content-addressed scenegraph protocol — talking to native FreeBSD kernel drivers via the Atrium GPU ABI. No DRM/KMS, no linuxkpi.
- **Distribution** is jail trees. Install = unpack into Tessera (deduplicates automatically). Update = swap tree (only delta bytes transfer). Rollback = re-tag. The package manager is **Opifex**.

These four properties hold simultaneously, and **no other commodity OS offers all four**.

For the full naming reference (Castellum, Vestibulum, Lyra, Tabula, Praeco, Curia, Scrinium, Forum, ...), see [NAMING.md](NAMING.md).

## Why now

Three diagnoses make this the right moment.

### 1. linuxkpi is a square peg in a round hole

FreeBSD's current GPU stack is `drm-kmod` riding on `linuxkpi` — a Linux kernel API emulation layer hosting near-unmodified Linux DRM/KMS drivers. It works, and FreeBSD wouldn't have modern GPU support without it. But:

- It pins FreeBSD's release cadence to Linux's.
- Every emulation gap is a FreeBSD bug.
- Subtle Linux-isms (RCU, completions, struct device semantics, locking ordering) leak.
- It puts FreeBSD permanently in second-class position — interesting work happens upstream-of-Linux, FreeBSD ports it.
- Doubles the kernel attack surface.

The reason it exists is pragmatic, not architectural: writing native FreeBSD GPU drivers from scratch in the traditional model means matching Vulkan/OpenGL feature parity (~1M LoC per GPU family per Mesa). Nobody is going to do that.

**Fresco changes the math.** The kernel/userspace surface needed to support a Fresco-style scenegraph stack is a fraction of DRM/KMS+Mesa. Native FreeBSD drivers become human-scale work, not vendor-dependent rewrites.

### 2. The retained-mode insight

Vulkan, OpenGL, Metal all express graphics as **per-frame imperative command streams**. Bandwidth and CPU work scale with scene complexity, every frame, regardless of how little changed.

Fresco's protocol is **retained-mode + content-addressed**: the server holds the scene tree, clients send mutations. Bandwidth scales with *changes*, not *content*. A 4 MiB glyph atlas uploads once for the whole desktop and is referenced by hash thereafter. A scene with 1 000 unchanged objects sends ~0 bytes per frame.

That single decision unlocks:

- Cross-process composition without buffer passing.
- Cross-machine graphics with bandwidth-proportional-to-mutations (not pixels).
- Server-wide caching (one font for the whole system).
- Vastly smaller kernel/userspace ABI.

### 3. Jails + Tessera are FreeBSD's hidden ace

FreeBSD has had jails as a first-class kernel primitive since 1999. Capsicum since 2010. They are stronger than Linux namespaces+cgroups, simpler than Linux LSMs.

**Tessera** — Atrium's content-addressed filesystem — makes the per-jail "ship your own libc" property cheap. Identical files are stored once globally as tesserae (tiles); jails see independent mosaics but the bytes are shared.

Together they enable an app distribution model that combines the strong-isolation property of macOS / Android / iOS with the runtime-freedom property of Nix / Guix, with the disk-cost property of OSTree, with the cross-process-graphics-isolation property of Fresco.

Linux can do most of these individually (flatpak, snap, OSTree, ostree-style CAS) but the integration is rough — each layer is independently conceived.

## The integrated stack

```
┌─ Apps (jailed by Portcullis) ─────────────────────────────────┐
│  atrium-edit, atrium-term, browser, ...                       │
│  Each app:                                                    │
│    - runs in a jail (FreeBSD kernel-level isolation)          │
│    - sees its own filesystem tree (Tessera-backed), libraries │
│    - declares capabilities in atrium.toml manifest            │
│    - opens /dev/fresco0 (Fresco protocol cdev)                │
└───────────────────────────────────────────────────────────────┘
                              │ Fresco protocol (per-slot rings)
                              ▼
┌─ Fresco display server (privileged userspace, runs as _atrium)┐
│  - retained-mode scene graph                                  │
│  - content-addressed storage (CAS, in-memory + on disk)       │
│  - per-window FBO compositing                                 │
│  - WM (decorations, focus, drag/resize, close)                │
│  - per-client slot isolation (cmd/comp/input rings per slot)  │
│  - input routing by target_window owner                       │
└───────────────────────────────────────────────────────────────┘
                              │ Atrium GPU ABI (small ioctl set)
                              ▼
┌─ Native FreeBSD kernel drivers (atrium-kmod) ─────────────────┐
│  /dev/atrium-gpu0       per vendor: virtio-gpu, AMD, ...      │
│  /dev/atrium-display0   modesetting, cursor, vblank           │
│  /dev/fresco0           Fresco protocol transport (per-slot)  │
│  HID via usbhid/hkbd, no evdev.                               │
│  No linuxkpi. No DRM/KMS. Pure newbus/kqueue/cdev.            │
└───────────────────────────────────────────────────────────────┘

  ── parallel system services (over Castellum IPC) ──
┌────────────────────────────────────────────────────────────────┐
│  Portcullis  (jail launcher + capability gate)                 │
│  Castellum   (system IPC bus)                                  │
│  Vestibulum  (display manager / login)                         │
│  Lyra        (audio)                                           │
│  Tabula      (clipboard)                                       │
│  Praeco      (notifications)                                   │
│  Opifex      (package manager)                                 │
│  Curia       (settings)                                        │
│  Scrinium    (file picker / browser)                           │
│  Forum       (shell: wallpaper + statusbar + dock)             │
└────────────────────────────────────────────────────────────────┘

  ── parallel storage ──
┌─ Tessera ─────────────────────────────────────────────────────┐
│  - every file content-addressed by SHA-256 (a "tessera")      │
│  - jails see per-jail mosaics (directory trees of tesserae)   │
│  - 1000 jailed apps with their own libc/Qt/etc cost ~1×       │
│  - install = unpack into Tessera                              │
│  - update = swap tree pointer                                 │
└───────────────────────────────────────────────────────────────┘
```

## Subsystems

Each is documented in its own file.

- [Naming reference](NAMING.md) — canonical vocabulary for every component.
- [Graphics](subsystems/graphics.md) — Fresco protocol, native kernel GPU ABI, backend strategy.
- [Storage](subsystems/storage.md) — Tessera (CAS filesystem), dedup, jail filesystem trees, distribution.
- [Sandbox](subsystems/sandbox.md) — Portcullis (jails per app), capability manifest, privilege boundary, IPC channels.
- [Transport](subsystems/transport.md) — Fresco protocol over ivshmem / cdev / TCP / QUIC / future hardware.

## What's done (2026-04-28 baseline; updated 2026-05-08)

### 2026-04-28 baseline
- Fresco protocol: stable, retained-mode, content-addressed, multi-client. Per-slot ring isolation (cmd, comp, input).
- Server: macOS-host development environment, Metal backend, full WM (decorations, drag, resize, close), per-window FBOs, ellipsis title truncation, focus-routed input.
- Kmod: native FreeBSD, newbus/kqueue/cdev/libmd. No linuxkpi.
- libfresco / fresco-rs: pure-Rust + C client libraries.
- Two real apps: atrium-edit (text editor), atrium-term (terminal emulator with /bin/sh + vte).
- Multi-process desktop demo: editor + terminal as separate FreeBSD processes, isolated input, drag-to-move-and-resize, kernel-pty `TIOCSWINSZ` propagation verified via `tput cols`.
- DMA upload (one cmd vs ~36 000 inline chunks for a 4 MiB atlas).
- Client-disconnect cleanup: kmod toggles `slots_alive_mask`, server reaps orphaned windows.

### Through 2026-05-08
- **D0 + D1** native virtio-gpu driver (`atrium-virtio-gpu`) and bare-metal Fresco server. atrium-edit running interactively on FreeBSD-native via the full Fresco protocol over Unix socket; native HID-keyboard input.
- **D1.5 Tessera** content-addressed filesystem: in-kernel `tessera_fs.ko`, full POSIX (pjdfstest sweep), mmap/exec, snapshots via `/.tessera/snapshots/<gen>/` magic dir, multi-extent packs, Git-style background repack, in-memory CAS read cache, perf matches/beats ZFS on multi-write fsync.
- **D1.7 binsplit Phase 1** — function-level dedup tooling (`tessera-binsplit --analyze | --compare | --multi`); 1.89× aggregate compression across 9 Atrium aarch64 binaries.
- **D1.6 aqueduct substrate** (`aqueduct/`: Connection / envelope / CAS / classes registry); `aqueduct-echo` smoke; **`CLASS_PORTCULLIS = 6`** registered with portcullisd as the first production consumer.
- **D2.5 Portcullis core** (privsep architecture live end-to-end):
  - `atrium-jaild` (privileged jail broker; pdfork + EVFILT_PROCDESC; SCM_RIGHTS for procdesc handoff; runtime AttachMount/DetachMount; orphan reconcile on restart).
  - `atrium-volumes` (separate allocation broker; tessera/plain/tmpfs plugins; idempotent provision; first-run init sentinels).
  - `atrium-portcullisd-daemon` (aqueduct cap mediator; peer-uid → manifest cross-check; manifest `[capabilities]` gate).
  - `atrium-portcullisd-bootstrap` (manifest-driven launcher + supervisor; failure-budget retries; tombstone retire; graceful SIGTERM/SIGINT shutdown).
  - rc.d scripts for all four daemons; production-shape boot via `service`.

## What's planned

See [ROADMAP.md](ROADMAP.md). Phases D0..D7, roughly:

- D0 — Native FreeBSD kernel GPU ABI + virtio-gpu driver. **Replaces linuxkpi+drm-kmod for our targets.**
- D1 — Native FreeBSD bare-metal server (Vulkan as transitional fallback for hardware not yet covered by D0).
- D1.5 — Tessera (CAS-FS port).
- D2 — Vestibulum (display manager + auth).
- D2.5 — Portcullis (jail launcher + capability manifest).
- D3 — Forum (shell: wallpaper, statusbar, dock) + Praeco (notifications).
- D4 — Foundation apps (atrium-files, Curia settings, atrium-image, atrium-pdf).
- D5 — Slint backend for Fresco (any Slint app gets the platform for free).
- D6 — Browser (Servo + WebRender → Fresco).
- D7 — Standardization push.

## Comparison

| Property | Linux + Wayland + flatpak | macOS | Android | FreeBSD + linuxkpi (today) | **Atrium** |
|---|---|---|---|---|---|
| Strong isolation | LSMs + namespaces (stitched) | App Sandbox (yes) | UID per app | None (native), inherits Linux's | **Jails (kernel-level) via Portcullis** |
| Per-app runtime freedom | Yes (flatpak runtimes) | No (system frameworks shared) | Partial | Yes | **Yes (full)** |
| Disk cost of N apps | Chunk-dedup partial (OSTree) | N× (per-app frameworks) | Per-app duplication | N× | **~1× (Tessera file dedup)** |
| Graphics isolation | Wayland (weak server-side) | App Sandbox + WindowServer | SurfaceFlinger | None | **Per-slot rings + window-owner enforcement (Fresco)** |
| Per-frame bandwidth | Vulkan command streams | Metal command streams | OpenGL/Vulkan | (same as Linux) | **Mutation deltas only (Fresco)** |
| Cross-machine | RDP/VNC (full pixel re-encode) | Same | Same | Same | **Native protocol, mutation-scaled (Fresco)** |
| Native kernel GPU drivers | Yes | Yes | Yes | **No (linuxkpi+drm-kmod retrofit)** | **Yes (native, atrium-kmod)** |
| Standardized | Wayland + Vulkan + DRM (3) | Closed | Partial | Inherits Linux | **In progress (D7)** |

## Strategic posture

This is a **multi-year program**. Not a single sprint. Honest scale:

- Year 0–1: validate native kernel GPU ABI on virtio-gpu + Raspberry Pi / Mali (small GPUs); ship D0..D3 (boot, login, shell, foundation apps).
- Year 1–2: add Tessera, Portcullis, capability manifest. First real "Atrium desktop" demo.
- Year 2–3: Slint backend (D5). Ecosystem inflection.
- Year 3–5: Servo-based browser (D6). One desktop-class GPU (AMD likely) via partnership.
- Year 5+: Standardization, more vendor partnerships, Apple Silicon (via Asahi-style RE), eventually Intel & NVIDIA.

The architectural risks are mostly behind us. What remains is engineering volume + ecosystem politics. Time is not the constraint; clarity of architecture and discipline of execution are.

## Audience

This isn't just a graphics-stack project. It's an OS-platform project. Conversations to seed:

- **FreeBSD foundation + core team** — they've wanted a way out of the linuxkpi position for years. A credible native alternative is a real story.
- **Jail / Capsicum people** — capability-based desktop is the natural extension.
- **Slint, Servo, embedded GPU vendors (Imagination, ARM Mali, Adreno)** — early ecosystem allies.
- **Cloud-desktop / VDI vendors (Frame, Cameyo, Nutanix)** — natural fit for the protocol-over-network property.
- **Khronos / FreeDesktop.org** — eventually for standardization.
- **Asahi Linux** — model for reverse-engineered driver approaches; potential cross-pollination on Apple Silicon.
