# Atrium

FreeBSD-based desktop platform built around four architectural decisions, integrated rather than bolted-on:

- **Graphics**: a retained-mode, content-addressed scenegraph protocol (**Fresco**) speaking to native FreeBSD GPU drivers via the **Atrium GPU ABI**. No DRM/KMS, no linuxkpi.
- **Storage**: file-level content-addressed dedup (**Tessera**). 1 000 jailed apps cost ~1 app's worth of disk.
- **Sandbox**: every app is a FreeBSD jail (**Portcullis** is the launcher). Capability manifest. Kernel-level isolation, not LSM-stitched.
- **Distribution**: jail trees. Install = unpack into Tessera. Update = swap tree (delta only). Rollback = re-tag.

These four properties hold simultaneously, and no other commodity OS offers all four.

For the full thesis see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). For canonical naming (Atrium, Fresco, Tessera, Portcullis, Castellum, Vestibulum, Lyra, Tabula, Praeco, Opifex, Curia, Scrinium, Forum) see [docs/NAMING.md](docs/NAMING.md). For the language stance see [docs/LANGUAGE-POLICY.md](docs/LANGUAGE-POLICY.md).

## Status

As of 2026-10-02 (the last commit), these pieces work:

- **D0 / D1**: a native virtio-gpu kmod through scanout, and `frescod` running Fresco on FreeBSD with atrium-edit interactive over the full protocol on a Unix socket.
- **D1.5 Tessera**: the in-kernel CAS filesystem (`tessera_fs.ko`), with POSIX behaviour checked by pjdfstest, mmap/exec, snapshots, and fsync performance matching ZFS. July to September went into hardening it: crash recovery, GC correctness, metadata-reserve exhaustion returning ENOSPC cleanly, snapshot pinning, quota and dedup domains, `tessera-fsck` / `tessera-repack`, and a namespace verifier that runs after every commit (on by default).
- **D1.6 Aqueduct**: the IPC substrate (envelope, CAS payloads, class registry).
- **D2.5 Portcullis core**: privsep jaild + atrium-volumes + portcullisd + supervisor, rc.d scripts, and capability-mediated runtime mounts. Every jail has its own root, user apps get per-app overlay volumes, and each app runs on its own vnet stack (point-to-point epair, NAT out, pf-isolated) with a synthetic host identity. jaild creates one-shot jails directly.
- **Tier-2 software Vulkan**: the `atrium-spv-*` SPIR-V compiler (bespoke ARM64 and Cranelift backends) behind `atrium-vk-icd`, held to byte-identical output by a differential test suite.
- **D6 Navigator (the JS-free document lane)**: M0 (Markdown to NSG, byte-identical across machines) and M1 (a jailed fetcher that can reach the network but cannot name a file) have passed. In M2 the strict profile renderer matches all 64 property rows, the normalizer brings the real-document corpus to 29/29, images paint from the CAS store, and WPT reftests serve as an oracle.
- **Started, not finished**: Stoa (persistent sessions), Lyra + Choragus (audio on the Laminar deadline scheduler), memoryd / memfed (memory pressure), atrium-gpu-amd (RDNA2), Pergola (the native toolkit) with first apps including the Vestibulum login screen, and the Forum shell pieces (`forum-wm`, `forum-bar`, `forum-dock`).

Still ahead: D2 Vestibulum login and session handoff end to end, the D3 Forum shell with the capability prompt UI (Portcullis phase 5), Fresco onto the Aqueduct envelope (`docs/spec/fresco-production-rollout.md`), D4.5 declarative animation, the D5 atrium-mesa fork and CLASS_AX accessibility, and Navigator's semantic tree.

See [RUNBOOK.md](RUNBOOK.md) for the bring-up procedure (`--virtio-gpu --display` QEMU flags, the `vtgpu` disable hint, host cross-compile setup) and [docs/ROADMAP.md](docs/ROADMAP.md) for per-phase scope and remaining work.

## Layout

The tree is a flat set of crates and kmods, grouped by prefix:

```
atrium/
├── docs/                    architecture, roadmaps (ROADMAP.md D0..D7, ROADMAP-INSULA.md), specs in docs/spec/
│
├── atrium-kmod/             native FreeBSD virtio-gpu driver (C, no linuxkpi)
├── atrium-gpu-rs/           safe Rust binding for the Atrium GPU ABI
├── atrium-gpu-amd/          native AMD (RDNA2) GPU module
├── atrium-vk-icd/           Vulkan ICD over the Tier-2 software renderer
├── atrium-spv-*/            SPIR-V frontend, IR, backends (bespoke ARM64, Cranelift), runtime, tests
│
├── fresco-*/, frescod/      Fresco protocol, scene server, text, Vulkan backend, the FreeBSD-native server
├── libfresco/               C client library
├── aqueduct*/               IPC substrate, conformance, paravirt GPU (guest, client, macOS host)
├── carillon-*/              doorbell-driven GPU VM transport
│
├── atrium-tessera/          Tessera content-addressed filesystem (kmod + tools)
├── portcullis/              jaild, portcullisd, atrium-volumes, launcher
├── stoa/                    persistent session service
├── lyra*/, choragus/        audio subsystem and audio policy
├── memoryd/, memfed/        memory-pressure daemon and per-jail memory federation
├── memprobe/                measures how real process memory compresses (evidence for atrium-zram v2)
│
├── pergola/                 native UI toolkit
├── vestibulum*/             login screen (+ macOS dev host)
├── forum-*/                 shell: window manager, bar, dock, overview, control
├── praeco-macos/, tabellarius-*/   notifications and push relay
├── navigator-*/             Navigator: fetch, normalize, style, render, prerender, fonts, backend
├── libatrium/               Insula platform library (C ABI for third-party apps)
├── atrium-*-socket/         sample apps on the socket path (edit, term, clock, find)
│
├── scripts/                 run-vm.sh, vssh, vshutdown, bootstrap-atrium.sh, test harnesses
├── etc/                     service manifests, devfs rules, pf, jail and volume policy
├── vm/                      QEMU EFI flash files (qcow2 disk excluded; see RUNBOOK)
└── test-assets/             sample files used by demos and tests
```

The companion [`atrium-os/fresco`](https://github.com/atrium-os/fresco) repo holds the Fresco protocol server (and `fresco-server` Rust crate, kept as the historical crate name). This tree's `frescod` and the socket-side libraries link it as a path dependency at `../../fresco`. Clone both at the same parent directory:

```sh
git clone https://github.com/atrium-os/atrium
git clone https://github.com/atrium-os/fresco
```

## Quick start

Per [RUNBOOK.md](RUNBOOK.md):

```sh
# Boot the FreeBSD VM with virtio-gpu + Cocoa display + USB keyboard:
scripts/run-vm.sh --virtio-gpu --display

# In another terminal, build everything host-cross (~30s first time):
cargo build --release --target aarch64-unknown-freebsd

# Inside the VM, set up + run:
vssh "kldload p9fs; mount -t p9fs -o trans=virtio bsd_share /mnt/host"
vssh "kldload /mnt/host/atrium-kmod/atrium_virtio_gpu.ko"
vssh "devctl set driver -f vtgpu0 atrium_virtio_gpu"
vssh "devctl enable atrium_virtio_gpu0"
vssh "nohup /mnt/host/frescod/target/aarch64-unknown-freebsd/release/frescod > /tmp/comp.log 2>&1 < /dev/null &"
vssh "nohup /mnt/host/atrium-edit-socket/target/aarch64-unknown-freebsd/release/atrium-edit-socket > /tmp/edit.log 2>&1 < /dev/null &"
```

Click the QEMU window and type — the editor responds.

## License

BSD-2-Clause unless noted otherwise. See `LICENSE`.

## Contributing

Atrium is a multi-year program; D0..D7 phases laid out in [docs/ROADMAP.md](docs/ROADMAP.md). The architectural foundation is in place; subsequent capabilities build on top. Every component has an entry-point doc in `docs/subsystems/`.

Issues and PRs welcome under `atrium-os/`.
