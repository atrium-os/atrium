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

## Atrium as a technology demonstrator

Atrium is **not** trying to replace Linux. It is trying to demonstrate
what a desktop OS designed in 2026 — from the kernel up, without
legacy commitments — would look like across every layer, and to let
those design choices flow back into other ecosystems by example. The
goal is influence-by-demonstration, not market share.

This framing matters because it changes what "success" looks like.
A platform aiming to replace Linux fails until it has 100M users. A
platform aiming to be a coherent next-generation demonstrator
succeeds when its design choices are visibly better and reproducibly
verifiable, regardless of how many users adopt it. BeOS, Plan 9,
NeXTSTEP, and Wayland are precedents — each one had small commercial
reach but outsized influence on what mainstream OSes eventually
adopted.

### What Atrium is demonstrating

Atrium ships **multiple coherent design statements**, each one
addressing a long-standing pain point in current desktop OSes. The
graphics stack is one such statement; it is not the only one.

| Subsystem | Mainstream pattern | Atrium's bet | Demonstrated by |
|---|---|---|---|
| **Storage (Tessera)** | Per-app file duplication; snapshots as a bolt-on (ZFS, btrfs) | Content-addressed FS where dedup, snapshots, and time-machine retention are *first-class* properties of how data is stored, not afterthoughts | `tessera_fs.ko` in-kernel, full POSIX (pjdfstest), `/.tessera/snapshots/<gen>/` magic dir, perf matches/beats ZFS on multi-write fsync (D1.5 ✓) |
| **IPC (Aqueduct)** | D-Bus and Unix sockets streaming raw bytes; expensive when sharing the same data across processes | Envelope-framed IPC with CAS-backed payloads — shared blobs dedupe across connections automatically | `aqueduct/` crate, `CLASS_PORTCULLIS = 6` already a production consumer, `aqueduct-echo` smoke (D1.6 ✓) |
| **Sandboxing (Portcullis)** | LSM-stacked security policy (SELinux/AppArmor) or container layer (Docker/Flatpak) — both bolted on outside the kernel's primary security model | Kernel-enforced FreeBSD jails with capability manifests; default-deny by construction, *the* security boundary, not a layer | Privsep architecture live end-to-end: `atrium-jaild`, `atrium-volumes`, `atrium-portcullisd-daemon`, `atrium-portcullisd-bootstrap`, rc.d scripts (D2.5 ✓) |
| **Package distribution (Opifex)** | dpkg / RPM / Flatpak with their own snapshot, dedup, rollback machinery layered on top | Install = unpack into Tessera (dedupes automatically); update = swap tree (delta bytes only); rollback = re-tag the FS snapshot | Spec at `docs/spec/atrium-pkg.md` and registry at `atrium-pkg-registry.md`; integrates Tessera (D1.5 ✓) + Portcullis (D2.5 ✓) |
| **Binary format (binsplit)** | Dynamic linking (libfoo.so.X) — fragile at runtime, ABI-locked, frequent breakage | Static binaries that dedupe at *function granularity* via CAS storage. Linking happens at build time; sharing happens at storage time | `tessera-binsplit --analyze --compare --multi` Phase 1; 1.89× aggregate compression across 9 Atrium binaries (D1.7 ✓) |
| **Graphics (Fresco + aqueduct-gpu)** | X11/Wayland + Mesa-as-runtime + drm-kmod + per-app shader trust | Retained-mode scenegraph protocol + universal shader sandbox + Mesa-as-build-tool + AOT shaders + closed wire vocabulary | aqueduct-gpu design in `docs/spec/aqueduct-gpu.md`; Phase 1+2 implementation in flight; frescod's Vulkan smoke proves the end-to-end path |
| **Input** | Linux evdev codes carried via libinput | USB HID usage codes natively, no evdev intermediate | atrium-keyboard, atrium-input spec; HID-codes-on-the-wire is the protocol contract |
| **Persistent sessions (Stoa)** | tmux + ssh, mosh, screen — all userland workarounds for a network-disconnect problem the OS doesn't address | Persistent SSH session service as a first-class OS daemon; graphical terminal falls out for free | Spec at `docs/spec/stoa.md`; D2.7 implementation deferred but architected |
| **Language posture** | Linux is mostly C; Rust slowly arriving as opt-in | Rust userspace + C kernel from day one, public APIs as C ABI for stability | `docs/LANGUAGE-POLICY.md` locked 2026-04-28; every Atrium-authored userspace crate is Rust |
| **License posture** | GPL kernel + mixed LGPL/permissive userland + proprietary vendor blobs | Permissive only in runtime (BSD/MIT/Apache); no GPL, no LGPL in the runtime image; closed drivers excluded by policy | `docs/LICENSING-POLICY.md`; the Mesa fork (`atrium-mesa`, MIT) replaces drm-kmod (GPL inherited from Linux) |
| **Kernel-userspace fault line** | Linux DRM in kernel + libdrm + Mesa in userspace + per-app driver instances | Native FreeBSD kernel GPU drivers (atrium-gpu ABI); Mesa only at build/install time, never in a runtime app process | D0 atrium-virtio-gpu kmod ✓; aqueduct-gpu design (`docs/spec/aqueduct-gpu.md`) defines the userspace half |

The coherence across this matrix is itself the meta-demonstration. The
*same* design principles — content-addressing, capability manifests,
permissive licensing, native FreeBSD primitives, no Linux-shape
leakage — show up in storage, IPC, package distribution, binary
format, graphics, and sandboxing. An app installed by atrium-pkg
lives in a Portcullis jail, reads from Tessera (which deduplicates
its binary at function granularity via binsplit), communicates over
aqueduct (whose payloads are themselves CAS-deduplicated), and
renders through aqueduct-gpu (whose shaders are AOT-compiled and
stored in Tessera by hash). Every layer reinforces every other
layer. That is the design statement.

### The discipline: every phase exit produces a demonstration

The strategic shift in being a technology demonstrator is that
**engineering deliverables and demonstration artifacts are the same
thing**. A phase exit is not "the test passes," it is "there is a
recordable, reproducible, shareable thing that demonstrates the
design choice."

Concretely, phase exit criteria are augmented with:

- A **reproducible benchmark** with the methodology published and the
  code public (target: a reviewer can re-run on their hardware and
  get within ±10% of our numbers).
- A **side-by-side comparison artifact** with the equivalent on
  Linux/macOS — screenshot, video, log, or perf number — where the
  comparison is *honest* (call out where we lose, not just where we
  win).
- A **30-to-90-second video** or animated screenshot that
  communicates the design choice without prose. Each demo should
  stand alone on social media.
- One **written explainer** (blog-post-shape, 1k–3k words) that ties
  the demonstration to the design choice it embodies, with code
  links and reproducibility notes.

This is more work than "ship the feature, write a test, move on." It
is also what turns engineering output into the platform's actual
deliverable. Every Atrium milestone should be a thing someone outside
the project can point at and say "that's a real design statement,
backed by working code."

### What this looks like across the upcoming arc

Pulling from the implementation phases of various subsystems:

- **Aqueduct-gpu Phase 1 + 1.5** (next 8–12 weeks). Demonstration:
  vestibulum rendering visibly nicer than Wayland/X11 equivalents;
  reproducible benchmark vs Linux+radv; "sandbox rejects a malicious
  shader" 30-second video.
- **Aqueduct-gpu Phase 2 + Bevy backend** (12–24 weeks). Demonstration:
  a real Bevy game running on Atrium with first-frame latency
  measurably better than the same game on Linux+radv; reproducible.
- **Tessera storage demo cycle** (parallel, 8 weeks). Demonstration:
  install 50 jailed apps; show disk usage; compare to the same 50
  apps on Linux with Flatpak; the difference is graphical.
- **Portcullis capability demo** (parallel, 4 weeks). Demonstration:
  an app that *cannot* read your files because its manifest didn't
  declare that capability; show the system-call returning EPERM at
  the kernel boundary, not at an LSM hook.
- **Aqueduct IPC dedup demo** (4 weeks). Demonstration: ten
  applications all displaying the same large image; ten connections;
  one CAS blob on the wire. Show the byte counter.
- **Fresco crash-recovery demo** (1–2 weeks, rollout M2.5).
  Demonstration: `kill -9` the display server; desktop re-renders
  with windows in place within 500 ms and no app exits — a direct
  consequence of retained-mode + CAS (clients replay their
  declarative scene). Side-by-side with Wayland (every app dies)
  and X11 (session gone). Spec: `docs/spec/fresco-recovery.md`.
- **Showcase content phase** (12–24 months parallel). An
  AAA-quality 30-minute experience, a Veloren port via the Bevy
  backend, ray-tracing demos. The "look at what runs here" tier.

Each row produces a demonstration artifact that stands alone *and*
contributes to the holistic argument that Atrium's design choices
fit together coherently.

### The audience for this demonstration

(Existing "Audience" section below remains; this paragraph reframes
the *purpose* of those conversations.)

The demonstrations are aimed at three audiences:

1. **System architects and OS researchers** — to influence what the
   next generation of Linux, macOS, or new platforms eventually
   adopt. The win condition is "Mesa upstream decides to land an
   AOT-by-default mode after seeing Atrium's install-time pre-compile
   pass; Wayland adds CAS-backed surface sharing after seeing
   aqueduct-gpu's surface-share; FreeBSD's jail subsystem grows
   capability-manifest support after Portcullis demonstrates it."
   This is "influence-by-demonstration" working.
2. **Developers of new engines, toolkits, and apps** — Bevy, Godot,
   small open-source 3D engines, the Rust graphics community. The
   win condition is "Bevy adds an aqueduct-gpu backend because
   Atrium's protocol is the cleanest GPU API anyone's actually
   implemented." A platform-as-design-reference, not necessarily a
   platform-they-deploy-to.
3. **The small set of users who want a coherent permissive desktop**
   — embedded device builders, paranoid orgs, license-strict
   enterprises, FOSS purists. The win condition is "Atrium runs as a
   shipped product in some specialised niche where its design choices
   are exactly what's needed (perhaps embedded HMI, perhaps secure
   workstations, perhaps thin-client compositors)." Steam Deck's
   analogue: not market-share-replacement, but defended niche.

If any one of these audiences is moved, the technology demonstrator
succeeded. If all three are moved, Atrium is the most influential
desktop-OS demonstrator since BeOS.

---

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
