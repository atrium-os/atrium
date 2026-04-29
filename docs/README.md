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

See [ARCHITECTURE.md § What's done](ARCHITECTURE.md#whats-done-2026-04-28).

## What's next (immediate)

**D0 — Native FreeBSD kernel GPU ABI + virtio-gpu driver.** First-class native graphics stack on FreeBSD with no linuxkpi dependency. See [ROADMAP.md § D0](ROADMAP.md#d0--native-freebsd-kernel-gpu-abi--virtio-gpu-driver).

After D0, the architectural foundation is complete and every subsequent phase (display manager, shell, foundation apps, Slint, browser) builds on a wholly native FreeBSD graphics stack.
