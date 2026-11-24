# AGENTS.md — rules for any coding agent working in Atrium

Atrium is a FreeBSD-based desktop platform (16.0-CURRENT, arm64 first, x86_64
port in progress). This file holds the project rules every agent needs.

The repo is `github.com/atrium-os/atrium`, cloned on the Mac at `~/src/bsd`
(scripts and `.cargo/config.toml` assume that path). The companion
`atrium-os/fresco` repo sits beside it at `~/src/fresco`; FreeBSD sources
are `atrium-os/freebsd-src`, branch `atrium-os`, in `freebsd-src/`.

The long-form references are:

- [README.md](README.md): what Atrium is, tree layout, quick start.
- [RUNBOOK.md](RUNBOOK.md): how to build, boot, deploy and test everything
  (§0 bootstrap, §2 VM lifecycle, §4 cross-compile, §5 quirks, §13 cloud and
  remote workflows).
- [docs/HANDOFF.md](docs/HANDOFF.md): **start here when picking up work.**
  Every open workstream with its exact next step and what it waits on.
- [docs/STATUS.md](docs/STATUS.md): the long-form status of each workstream.
- [docs/spec/](docs/spec/): one spec per subsystem (about 145). Read the spec
  before changing a subsystem, and update it in the same commit when the
  design changes.
- [docs/NAMING.md](docs/NAMING.md): the canonical component names.

## Design in one page

- **Kernel**: FreeBSD with our patches (`freebsd-src`, branch `atrium-os`),
  the Laminar scheduler, and Atrium kmods: Tessera (`tessera_fs.ko`,
  content-addressed filesystem, the default root), native GPU and display
  drivers, Hospes, Aura (Wi-Fi). No linuxkpi, no DRM/KMS, no shims: use
  BSD-native primitives.
- **Every app is a jail.** `jaild` is the only caller of `jail_set(2)`;
  `portcullisd` reads the bundle's signed `atrium.toml` manifest and enforces
  its capabilities. The jail rootfs is the bundle. Each app gets its own
  overlay volume and its own vnet network stack.
- **System services are small, jailed, privilege-separated Rust daemons**,
  each with one job and its own uid: Arca (the secret store: apps *use*
  secrets, never read them), Vestibulum/Ostiarius (login, session), Stoa
  (persistent sessions), Lyra/Choragus (audio), Curia (settings), Scrinium
  (files), Praeco (notifications), Tabula (clipboard), Opifex (packages),
  Monstrator (links), Prelum (printing), Crypta (VPN), Dispensator (devices),
  Capsa (removable media), Navigator (the web, `navigatord` + jailed engines).
  Parsing of untrusted input happens in a jail with no secrets and no network
  it does not need.
- **IPC**: Aqueduct (envelope + class registry + CAS payloads). **Graphics**:
  Fresco (retained-mode, content-addressed scenegraph) served by `frescod`;
  apps build UI with Pergola. **Shell**: Forum (`forum-wm`, `forum-bar`,
  `forum-dock`, ...).
- Treat every client of a service as hostile. Wire formats have one profile:
  the hostile one.

## Hard rules

- **Licensing: permissive only.** No GPL, LGPL, AGPL, SSPL, BUSL, CDDL or EPL
  code or dependency in anything that ships. MIT, BSD, Apache-2.0, ISC, Zlib,
  CC0 are fine; MPL-2.0 is case by case. Check the license of every new crate
  before adding it. See [docs/LICENSING-POLICY.md](docs/LICENSING-POLICY.md).
- **Language**: userspace is Rust, new kernel code is Rust by default (C for
  VFS/newbus glue and code imported from FreeBSD), public APIs are C ABI.
  See [docs/LANGUAGE-POLICY.md](docs/LANGUAGE-POLICY.md).
- **Build on the host, never in the VM.** The VM is for running and testing.
  Never run `cargo build --release` (or any release-profile cargo) inside the
  VM: it hangs the guest and `kill -9` of QEMU corrupts the qcow2.
- **Never `execve` a binary off the 9p share** (`/mnt/host`): it panics p9fs.
  Stage with `scp` and run from the guest's local disk.
- **Proof of concept: features over stability.** Target 16.0-CURRENT and use
  the native FreeBSD API rather than portable fallbacks.

## Building and testing

There is no top-level Cargo workspace: each crate builds on its own from its
directory. The toolchain is pinned in `rust-toolchain.toml` (nightly with
`rust-src`, since `aarch64-unknown-freebsd` is Tier 3 and std is rebuilt).
Move the pin only on purpose, for the whole tree.

```sh
sh scripts/bootstrap-atrium.sh                    # bare checkout -> sysroot, QEMU, kernel, kmods, userspace, image
cd <crate> && cargo test                          # host unit tests
cd <crate> && cargo build --release --target aarch64-unknown-freebsd   # cross-build for the guest
sh scripts/core-host-tests.sh                     # Tessera core C tests, plain + ASan/UBSan
```

- `.cargo/config.toml` holds the Mac cross-link setup (rust-lld + the FreeBSD
  CRT from `sysroot/`). In cloud sessions use `. scripts/cloud-vm.sh env` and
  pass `RUSTFLAGS="$CROSS_RUSTFLAGS"` per command, never globally.
- Kernel and kmods cross-build on the host with FreeBSD's
  `tools/build/make.py` (RUNBOOK §4, §13.2). Never run two `make.py` builds on
  one objdir.
- Most subsystems have a VM check script in `scripts/` (`*-vm-check.sh`,
  `*-gate.sh`, `*-vm.sh`). Run the one for the area you changed; the RUNBOOK
  section for that subsystem names it.
- A new check script sources `scripts/lib/gate.sh` (`ok`/`bad`/`check`,
  `gate_begin NAME COUNT`, `gate_end`) so it exits non-zero on a failure and
  fails when fewer checks ran than it declared.
- Cargo intermediates share `$CARGO_HOME/atrium-build` (default
  `~/.cargo/atrium-build`) across the main checkout and scratch worktrees.
  Final binaries are still in each crate's `target/` directory. Concurrent
  builds may wait on the shared directory lock.
- Scratch worktrees: make them with `scripts/scratch-worktree.sh` under a
  dedicated parent and remove them with `--remove`, never `rm -r`.

## The dev VM

- Create disposable test VMs when a check needs them. After the run, shut
  them down and remove their temporary disks once no remaining VM depends on
  them. Keep logs, screenshots, and test results as evidence. Use
  `scripts/cloud-vm.sh test` for the Sheets, Navigator, and Mail cloud gates.
  Custom runners can call `scripts/disposable-vm-cleanup.py` with their run
  directory and disk filenames after QEMU exits (see RUNBOOK).
- `scripts/run-vm.sh` boots it, `scripts/vssh` runs commands in it,
  `scripts/vshutdown` stops it. Always stop it with `vshutdown`.
- **The Mac VM is shared by several agents working in parallel.** Don't boot,
  reboot or stop it without an OK typed by Shankar (inside a Claude project,
  ask the coordinator), and never stop a VM someone else started. `vshutdown` refuses when more than one VM runs or a
  test harness owns the VM; stop the harness instead, since killing a harness
  can relaunch or kill its VM.
- `vssh` has a 3 s connect timeout and fails with empty output when the guest
  is busy; use `VSSH_CONNECT_TIMEOUT=30` for scripted work and check for a
  marker at the destination.
- On the Mac, macOS's daily `periodic` job `110.clean-tmps` (about 00:00
  IST) deletes anything under `/tmp` (`/private/tmp`) not accessed for 3
  days. Long-lived build trees and VM base images live under `~/atrium-vm`
  or `~/src`, never `/tmp`.
- In cloud sessions use `scripts/cloud-vm.sh` (QEMU under TCG; never quote its
  timings as product numbers). See RUNBOOK §13.

## Handing work over

Claude and Codex share this project and pick up each other's work. Neither
can see the other's conversations or memory, so the repo is the only
handoff. Before a session ends, runs out of budget, or passes a workstream
on:

1. Commit and push what you have (or say in the row exactly what is left
   uncommitted and where).
2. Update that workstream's row in [docs/HANDOFF.md](docs/HANDOFF.md): where
   it stands, the exact next step, and what it is waiting on. Keep the row to
   a few lines that link the spec's as-built section; history goes there.
   Add a row for a new workstream; move a finished one to "Recently
   finished".
3. Push. When picking work up, read the row, then the spec it links, then
   `git log` for that area.

## Git

- Work on `main` and push directly; there are no PRs for normal work. Several
  agents and cloud sessions push to `main` at the same time, so
  `git pull --rebase` before you commit and again before you push.
- Commit subjects name the component and say what changed in plain words,
  e.g. `Mail: filters against a real Pigeonhole, and a VM check for the
  Dovecot tests`.
- Never commit secrets, build outputs (`target/`, `.ko`, images, tarballs) or
  anything under `scratch/` unless asked.

## Review checklist for new code

Before pushing, check your diff against these. An adversarial reviewer
thread checks them too.

- Every value from a less-trusted party is bounded before it is buffered:
  frame and line lengths, counts of pending items, declared sizes.
- No `unwrap`, `expect`, slicing or `as` casts on bytes a peer controls.
  Return an error and drop that peer.
- An error on an authority check denies. Never `unwrap_or_default()` a
  file or answer that grants access; never ignore the result of a
  `set_permissions`/`chown` that is the access control.
- Text handed to another parser (jail.conf, fstab, shell, pf, TOML,
  registry lines) is built from validated tokens, and the renderer refuses
  rather than escapes anything else.
- Root never writes a predictable path in a shared directory: `O_EXCL`,
  0600, fresh name, or a root-only directory.
- Reuse the shared helper (fd passing is `atrium-fdpass`; peer uid,
  framing, hex) instead of writing a new one.
- Every `unsafe` block has a `SAFETY:` comment saying why it holds.
- A comment states the invariant, not the history; history goes in the
  commit message.
- A test asserts something, and fails on the code before the fix.
- A new or updated dependency passes `python3 scripts/licence-audit.py
  <crate dir>`.
- No new code in portcullisd for one app's feature: ask whether the
  owning service can do it with a generic primitive.
  See [docs/reviews/2026-11-24-codebase-review.md](docs/reviews/2026-11-24-codebase-review.md).

## How to verify (lessons that cost real time)

- A green suite can hide a hang: reconcile the test names that printed
  against the ones that should have. A SIGKILL is a bug report.
- One timing is not a measurement: at least 3 runs per arm, and control the
  dominant variable before comparing.
- Check which kernel and which binary you actually measured.
- Positive-control every counter and probe: make it fire once on purpose.
- Assert that the new input landed, not just that output appeared.
- A privilege drop must verify itself (uid, gids, groups) after dropping.
- A named kernel object (devfs ruleset, sysctl, module) must exist: ask the
  kernel, don't trust the config file.
- Silent-drop paths (an unknown case that returns empty or `EIO`) hide bugs;
  make them loud.
- Bound every wait loop.
- A hung guest: capture it with QEMU's `dump-guest-memory` before resetting
  it, and walk the dump offline (see `scripts/vm-reserve-exhaustion-test.sh`).
