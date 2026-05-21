# Atrium — documentation

Architecture and design docs for an integrated FreeBSD desktop platform built around:

- **Fresco** — retained-mode, content-addressed scenegraph protocol for graphics.
- **Tessera** — content-addressed filesystem for cheap per-app library trees.
- **Portcullis** — jail launcher; one FreeBSD jail per app, with a capability manifest.
- **Native kernel GPU drivers** (Atrium GPU ABI) — replaces FreeBSD's linuxkpi+drm-kmod retrofit.

## Read in this order

1. [NAMING.md](NAMING.md) — canonical vocabulary (Atrium, Fresco, Tessera, Portcullis, Castellum, Vestibulum, Lyra, Tabula, Praeco, Opifex, Curia, Scrinium, Forum). Read this first.
2. [ARCHITECTURE.md](ARCHITECTURE.md) — the integrated platform thesis. Why now, what's different, what's done, what's planned.
3. [ROADMAP.md](ROADMAP.md) — phased deliverables D0..D7, dependencies, honest scope.
4. Subsystem deep-dives:
   - [subsystems/graphics.md](subsystems/graphics.md) — Fresco protocol + Atrium GPU ABI.
   - [subsystems/storage.md](subsystems/storage.md) — Tessera + per-jail mosaics + distribution.
   - [subsystems/sandbox.md](subsystems/sandbox.md) — Portcullis: jails + capability manifest + IPC channels.
   - [subsystems/transport.md](subsystems/transport.md) — protocol over ivshmem / cdev / TCP / future hardware.
5. [ORGANIZATION.md](ORGANIZATION.md) — proposed GitHub repo layout (`atrium-os/*`), versioning, ports tree, IPC bus (Castellum), licensing.
5b. [LANGUAGE-POLICY.md](LANGUAGE-POLICY.md) — kernel = C, userspace = Rust, public APIs = C ABI. The reasoning, recorded once.
6. [spec/wire-format.md](spec/wire-format.md) — **normative wire-format specification (v0.1.0)**: record layouts, opcode registry (`FRESCO_*`), semantics, extension mechanism.
7. [spec/gpu-abi.md](spec/gpu-abi.md) — **Atrium GPU ABI specification (v0.1.0)**: cdev surface (`/dev/atrium-gpu0`, `/dev/atrium-display0`), ioctl registry (`ATRIUM_GPU_*`), BO lifecycle, modesetting.

## Operational reference

- [../RUNBOOK.md](../RUNBOOK.md) — current development environment (QEMU + macOS host), build commands, gotchas.

## What's done

- Fresco protocol + multi-client desktop POC (editor + terminal as separate FreeBSD processes).
- macOS-host development environment (Metal backend).
- Native FreeBSD kmod (no linuxkpi).
- Per-slot ring isolation (cmd / comp / input).
- DMA upload, drag-to-resize, kernel-pty `TIOCSWINSZ` propagation.
- **D0 + D1** native virtio-gpu driver and bare-metal Fresco server (atrium-edit running interactively over the full Fresco protocol).
- **D1.5 Tessera** content-addressed filesystem: in-kernel `tessera_fs.ko`, POSIX-compliant (pjdfstest sweep), mmap/exec, snapshots via `/.tessera/snapshots/<gen>/`, perf matches/beats ZFS on multi-write fsync.
- **D1.6 aqueduct substrate** (`aqueduct/`: Connection + envelope + CAS + classes registry); aqueduct-echo demo; `CLASS_PORTCULLIS = 6` registered with portcullisd as the first production consumer (2026-05-08).
- **D2.5 portcullis core** (2026-05-08): privsep architecture live end-to-end. jaild + atrium-volumes + portcullisd-daemon + portcullisd-bootstrap, all with rc.d scripts. Manifest schema with `[[volumes]]`, `[volumes.init]` first-run sentinels, `[capabilities]` block. Capability mediation via aqueduct (in-jail `uid 1001` services drive runtime AttachMount/DetachMount through the daemon, with peer-uid → manifest cross-check). Defense-in-depth mount cleanup (per-exit + graceful-shutdown + jaild orphan-reconcile). Per-jail rootfs trees deferred to D5 (smoke uses `path = "/"`); capability prompt UI deferred to D3 (Forum).
- **D5 Tier-2 software Vulkan compute path** (2026-05-21): bespoke ARM64 + Cranelift fallback both runtime-correct end-to-end through atrium-vk-icd. Multi-binding SSBO (1–6 bindings), dynamic AccessChain (`ssbo[i]`), all 12 standard SPIR-V atomic ops, 24 GLSL.std.450 math functions (incl. length, normalize, reflect, cross, smoothstep, fmod, inverseSqrt, sabs, sin/cos/tan with range-reduction modulo π so the full real line is accepted), real-workload integration (histogram, per-vertex Lambert lighting, vec4 Reinhard tonemap, normalize+scale chain, parallel sum-reduction) — 51 cross-backend differential tests + 18 vk-icd end-to-end tests byte-identical, 0 ignored. See [spec/tier2-renderer.md § 15](spec/tier2-renderer.md#15-status--next-steps).

See [ARCHITECTURE.md § What's done](ARCHITECTURE.md#whats-done-2026-04-28) and [ROADMAP.md](ROADMAP.md) for per-phase status detail.

## What's next (immediate)

The active fronts as of 2026-05-08:

- **Fresco-on-aqueduct** — migrate Fresco from the legacy 128-byte fixed-frame format onto the unified envelope. The substrate is ready; the cutover plan is `spec/fresco-production-rollout.md` M2.
- **D5 atrium-rootfs** — per-jail rootfs trees so manifests stop using `path = "/"`. Unblocks proper jail isolation testing and obsoletes the host-namespace mount workarounds.
- **D2 vestibulum** (login screen) and **D3 forum** (dock) — both build on the jail runtime that now exists.
- **atrium-mesa fork** (D5) — long-tail dependency for production-grade rendering.
