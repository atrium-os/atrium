# Atrium

FreeBSD-based desktop platform built around four architectural decisions, integrated rather than bolted-on:

- **Graphics**: a retained-mode, content-addressed scenegraph protocol (**Fresco**) speaking to native FreeBSD GPU drivers via the **Atrium GPU ABI**. No DRM/KMS, no linuxkpi.
- **Storage**: file-level content-addressed dedup (**Tessera**). 1 000 jailed apps cost ~1 app's worth of disk.
- **Sandbox**: every app is a FreeBSD jail (**Portcullis** is the launcher). Capability manifest. Kernel-level isolation, not LSM-stitched.
- **Distribution**: jail trees. Install = unpack into Tessera. Update = swap tree (delta only). Rollback = re-tag.

These four properties hold simultaneously, and no other commodity OS offers all four.

For the full thesis see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). For canonical naming (Atrium, Fresco, Tessera, Portcullis, Castellum, Vestibulum, Lyra, Tabula, Praeco, Opifex, Curia, Scrinium, Forum) see [docs/NAMING.md](docs/NAMING.md). For the language stance see [docs/LANGUAGE-POLICY.md](docs/LANGUAGE-POLICY.md).

## Status

D0 (native virtio-gpu kmod through scanout) and D1 (atrium-edit running interactively on FreeBSD-native via the full Fresco protocol over Unix socket) are working. See [RUNBOOK.md](RUNBOOK.md) for the bring-up procedure, including the `--virtio-gpu --display` QEMU flags, the `vtgpu` disable hint, and the host cross-compile setup.

## Layout

```
atrium/
├── docs/                    architecture, roadmap, specs (wire-format, gpu-abi)
│   ├── ARCHITECTURE.md
│   ├── ROADMAP.md           phases D0..D7
│   ├── NAMING.md            canonical vocabulary
│   ├── LANGUAGE-POLICY.md   kernel = C, userspace = Rust, public APIs = C ABI
│   ├── ORGANIZATION.md      proposed multi-repo layout under atrium-os/
│   └── spec/
│       ├── wire-format.md   normative Fresco protocol spec (v0.1.0)
│       └── gpu-abi.md       Atrium GPU ABI spec (v0.1.0)
│
├── atrium-kmod/             native FreeBSD virtio-gpu driver (C, no linuxkpi)
├── atrium-gpu-rs/           safe Rust binding for the Atrium GPU ABI
├── fresco-rs/               Fresco protocol Rust binding (libfresco-backed; ivshmem path)
├── fresco-socket-rs/        Fresco protocol Rust client over Unix socket
├── fresco-text/             rustybuzz + swash glyph atlas builder
├── libfresco/               C client library (ivshmem transport)
├── fresco-kmod/             kmod for the ivshmem transport (development)
│
├── frescod/       FreeBSD-native server (TinySkiaBackend + socket)
├── atrium-edit/             text editor (libfresco / fresco-rs path)
├── atrium-edit-socket/      text editor on the FreeBSD-native socket path
├── atrium-term/             terminal emulator (libfresco path)
├── atrium-clock/            analog clock (libfresco path)
├── atrium-find/             two-pane file browser (libfresco path)
├── atrium-test-client/      bring-up demos + wire-protocol harnesses
│
├── scripts/                 run-vm.sh, vssh, vshutdown
├── vm/                      QEMU EFI flash files (qcow2 disk excluded; see RUNBOOK)
└── test-assets/             sample files used by demos
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
