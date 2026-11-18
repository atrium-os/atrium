# Developer tools and app composition: implementation plan

**Status:** accepted 2026-11-17 (Shankar: "agreed with your recommendation", P1–P6
as recommended). Three threads: platform, terminal and Artifex, toolchains and SDK.
Implements [dev-tools.md](dev-tools.md) (T1–T10) and [app-composition.md](app-composition.md)
(A1–A8), both accepted 2026-11-17.

This plan orders the work into milestones, names the code each one touches, and
gives every milestone a gate script in the usual `scripts/vm-*-check.sh` style
(cloud amd64 VM through `scripts/cloud-vm.sh`; on-screen checks on the Mac VM where
a frame must be looked at). Sizes are relative: **S** a day or less of thread work,
**M** a few days, **L** a week or more.

## 1. The order at a glance

| # | Milestone | From | Size | Needs |
|---|---|---|---|---|
| **M0** | Syscall classes that let consoles and workspaces work under deny mode | T1 | S–M | — |
| **M1** | Terminal emulator complete enough for vi, Helix, Neovim | T2 | M | — |
| **M2** | `pkg` in a workspace | T3 | M | M0 |
| **M3** | Toolchain layers: format, `c-dev`, `rust` (both arches) | T4 | L | M2 (shared jail code) |
| **M4** | Run carries the full closure; C SDK in the base; `atrium.app.mk` | T5 | M | — |
| **M5** | `[[workers]]`: manifest, portcullisd, `atrium-worker` crate, Officina and Mail moved onto it | A1–A3 | L | M4 (Run with workers) |
| **M6** | Third-party Limen (`embeds`), providers from manifests, `arca` capability | A4, A7 part | M | M5 (manifest) |
| **M7** | `cargo atrium`, templates, `Message` derive and C generation, `check` lint, `fuzz` | A5, A6, T5 part | L | M3, M5 |
| **M8** | `registries` network mode | T6 | M | M0 |
| **M9** | Arca SSH agent and git credentials in workspaces | T7 | M | Arca A5 (agent) |
| **M10** | Run under debugger, cores | T8 | M | M0, M4 |
| **M11** | Artifex v0: editor, LSP, tasks, terminal pane, git status, composition panel, New project | T9, §6.1 | L+ | M1 (terminal), M3 (servers), M7 (templates) |
| **M12** | Artifex fuzz panel and debugger; SDK published (registry, sysroots, guide) | T10, A8, §6.1 | L | M7, M10, M11 |
| later | Command bundles; self-hosted Atrium build; Nuntius C ABI and libatrium level 1 calls | §4.7, §5.6, A7 | — | — |

M0 goes first because it is a latent break: today's workspaces, the empty console
and app consoles all run in jails whose syscall class has no `fork` or `wait4`, and they
work only because sysfil is in log mode (§2). M1 and M2 together give a usable
terminal development setup quickly. M5–M7 come before Artifex because Artifex's
New project and composition panel are built on them.

## 2. M0: syscall classes for consoles and workspaces (S–M)

**Problem, from the code.** `portcullis_jail::sysfil_class` (`portcullis/portcullis-jail/src/lib.rs:117`)
gives `networked` or `app` unless the manifest names a class. The manifests
portcullisd generates name none: workspaces (`portcullisd/src/workspace.rs`, the
`[capabilities] network` line), the empty console (`portcullisd/src/empty.rs:87`,
`network = "none"`, so `app`). App consoles join the app's own jail
(`console.rs`, `jail_attach`), whose class is the app's. None of `app`,
`networked` has `fork`, `vfork` or `wait4`; only `terminal` adds them
(`etc/jaild.policy.toml`), so a shell there can't start a command.

**Work.**
1. `etc/jaild.policy.toml`: a `develop` profile = `terminal` + `networked` +
   `ptrace`, `ktrace`, `procctl` (only own-uid targets are reachable anyway), and the
   calls lldb and sanitizers use (filled by `scripts/sysfil/learn.sh` runs of the
   gates below). jail-sysfil.md §5.1 gets the class row.
2. `workspace.rs`: `syscalls = "develop"` in the generated manifest; the jail
   gets `allow.unprivileged_proc_debug` (a `jc.set`). The host sysctl stays 0.
3. `empty.rs`: `syscalls = "terminal"`.
4. App consoles: see P1. Recommended: a **console jail** per app console, a child of
   nothing, with the app's root, overlay and uid mounted the same way, class
   `terminal`, so the app's own jail keeps its narrow class.
5. `CapabilityDelta` already treats `terminal` as a consent item; `develop` is only
   ever set by portcullisd for workspaces, so a manifest naming it is refused
   (`portcullis-toml` check), like restricted capabilities.

**Gate** `scripts/vm-sysfil-dev-check.sh`: policy mode 2 for these classes; then
`vm-workspace-check.sh`, `vm-terminal-check.sh`, `vm-console-check.sh`,
`vm-ide-check.sh`, `vm-run-check.sh` all pass; `lldb` sets a breakpoint in a test
binary in Scratch; an app jail's `ptrace` is denied; a manifest asking for
`develop` is refused.

## 3. M1: the Terminal (M)

**Code.** `stoa/stoa-term/src/lib.rs` (the `vte::Perform` grid; scope comment at
l.14–18 lists what is deferred), `stoa/stoa-term/src/diff.rs` (state diffs must
carry the alternate screen and modes), `atrium-term-socket/src/{main,keymap,render}.rs`
(mouse and paste encoding, OSC 52 to the clipboard capability).

**Work, in this order** (each with unit tests feeding byte sequences):
1. Scroll regions (DECSTBM) and scrolling inside them; IL, DL, ICH, DCH, ECH; tab
   stops (HTS, TBC); DECSC/DECRC; DECAWM; DECTCEM.
2. Alternate screen (1049, 1047, 47) with saved cursor; scrollback untouched by it.
3. Modes: DECCKM, bracketed paste 2004, focus 1004, mouse 1000/1002/1006 (SGR);
   the Terminal encodes clicks, drags and wheel only when a mode asks.
4. OSC 0/2 titles into the window title (bounded, control characters stripped);
   OSC 52 writes to the clipboard, never reads.
5. `TERM=xterm-256color` set by portcullisd for console shells.

**Gate** `scripts/vm-terminal-apps-check.sh`: recorded sessions of vi, `less`,
`top` (base), and Helix, Neovim (after M2) replayed into `stoa-term` and compared
with reference grids; on the Mac VM, frames of each editor open on a file, a
mouse click moving Helix's cursor, a bracketed paste arriving as one edit.
The recordings are made once on FreeBSD with `script(1)` and kept in `test-assets/`.

**As built (2026-11-17).** Done: `stoa-term` is a full xterm-style emulator
(scroll regions, insert/delete, tab stops, alternate screens, the modes above, mouse
1000/1002/1006, DA/DSR/DECRQM and OSC 10/11 replies, wide characters, OSC 52 writes
only); state diffs carry modes, title and one-shot clipboard text; stoad answers the
program's queries; the Terminal encodes xterm keys, mouse and bracketed paste, and
copies OSC 52 only with a copy-capable press (tabula.md §2.2). The recordings were
made with tmux as the oracle (`scripts/terminal-record.sh`, 8 sessions). The gate
passes on the cloud amd64 VM: vi (insert, motions, write), `less`, `top`, and the
`pkg` question (below). On the Mac VM (arm64, 2026-11-17) the same gate ran with
pkg installing Helix and Neovim into Scratch (exit 0): both open the file on the
alternate screen and quit cleanly, and a click moves Helix's cursor; bracketed
paste waits for Tabula in that VM. Found on
the way: consoles had no `/var/tmp`, so vi opened on a recovery-folder error that ate
the first key (the empty console now has one; app consoles set vi's `recdir`).

## 4. M2: `pkg` in a workspace (M)

**Code.** `portcullis/portcullis-protocol` (new `WorkspaceAdmin { name, argv,
activation }`), `portcullisd/src/workspace.rs` (handler), `portcullisd/src/main.rs`
(routing, press check as for `WorkspaceRun`), `atrium-workspaces` (the press UI),
`etc/jaild.policy.toml` (a `pkg` class: `develop` + `chown`, `chmod`, `chflags`
refused).

**Work.**
1. The handler checks the press, then runs the argv as **root inside the
   workspace's jail** (`jail_attach` + uid 0), only for `pkg` with an allow-listed
   first word (`install`, `delete`, `upgrade`, `info`, `search`, `autoremove`,
   `clean`), stdin/stdout back over the same descriptors as `WorkspaceExec`.
2. The workspace jail gains `allow.raw_sockets=0`, `allow.mount=0`,
   `allow.chflags=0`, `enforce_statfs=2` explicitly (most are defaults; asserted).
3. A `pkg` wrapper first on the workspace's `PATH` (a tiny binary in the console
   toolbox) that sends the request; `pkg` from a WorkspaceExec'd IDE works the same.
4. `/var/db/pkg` and `/var/cache/pkg` in the workspace root, root's; `pkg.conf`
   pointing at `pkg.freebsd.org` for `FreeBSD:16:<arch>` (P5 for the later Atrium
   mirror).
5. Workspaces app: "Install packages…" opens a console with the wrapper explained;
   no package UI yet.

**Gate** `scripts/vm-workspace-pkg-check.sh`: `pkg install helix` asks the press
and installs into that workspace only (another workspace doesn't have it); a
package whose post-install script writes `/usr/bin/x` fails (base read-only); the
host's `/var/db/pkg` is untouched; `pkg` without a press is refused; argv other
than the allowed words is refused.

**As built (2026-11-18).** Done, with the Terminal's half of the press
(homes.md §5.5: the wrapper asks with OSC 7770, stoad forwards it from the pane
with the keys, the Terminal asks "<workspace> asks to run: pkg …" with Install
and Cancel, sends `WorkspaceAdmin` and shows pkg's output over the tab;
`vm-terminal-apps-check.sh` checks the ask, Cancel and Install). Two changes from the plan. There is no
separate `pkg` class: a jail has one filter, so pkg runs under the workspace's
`develop` class, which gained `chflags`, `symlink` and `eaccess` (with
`allow.chflags` off, no system flag can be set). `update` and `remove` were added to
the allowed subcommands. The gate `scripts/vm-workspace-pkg-check.sh` passes 26 of
26 with the filter denying: pkg bootstraps and installs into one workspace only,
the host's `/var/db/pkg` is untouched, the jail's root can't write the base, mount
or set flags, and refused words stay refused. An app asking without a press isn't
tested yet, because that needs frescod.

## 5. M3: toolchain layers (L)

**Format.** A layer is a signed tree like a bundle (`atrium.toml` with `kind =
"layer"`, `[layer] name, version, arch, mount = "/atrium/toolchains/<name>"`,
`env = { PATH = "...", ... }`), stored in Tessera CAS, installed by Opifex
(`portcullis/opifex`: `opifex layer install|list|remove`), signed by the platform
key (`portcullis-sig`). The workspace record gains `layers = ["rust@1.91", "c-dev@1"]`;
`workspace.rs` mounts each read-only and writes the env into the console profile and
`WorkspaceExec`'s environment.

**Builds** (scripts in `scripts/layers/`, run in the cloud, results signed and put
in project files until a registry exists):
1. `build-c-dev-layer.sh`: `pkg fetch` the listed packages for each arch into a
   tree (pkgconf, cmake, meson, ninja, gmake, git, llvm's clangd and clang-format,
   bear, helix), check their licenses into the layer's `LICENSES.md`, pack.
2. `build-rust-layer.sh`: rust-lang/rust at a release tag, `x.py dist` cross-built
   from Linux for `aarch64-unknown-freebsd` and `x86_64-unknown-freebsd` hosts, with
   std for both targets, rust-src, clippy, rustfmt, rust-analyzer; sysroots from the
   cloud cross-build recipe (release ISO plus pkgbase dev packages) for both arches. The pinned
   nightly is built the same way. This is the longest single build (P2).
3. `TOOLCHAINS` in `workspace.rs` turns Rust on (Python, Go, Node follow the c-dev
   recipe from FreeBSD packages, later).

**Gate** `scripts/vm-layers-check.sh`: a Rust workspace runs `cargo new x && cargo
test`, `cargo clippy`, and rust-analyzer answers an `initialize` + `hover` over
`WorkspaceExec` pipes; a C workspace with `c-dev` builds a CMake and a Meson project
and `pkg-config --libs zlib` works; a tampered layer is refused at install; two
workspaces on the same layer share its blocks (Tessera dedup count).

## 6. M4: Run closure, C SDK, `atrium.app.mk` (M)

**Code.** `portcullisd/src/dev.rs` (the closure walk at l.206–300 copies base
libraries only), base image scripts (`scripts/deploy-portcullis.sh` and the image
builders) for the SDK files, new `etc/mk/atrium.app.mk`.

**Work.**
1. `dev.rs`: resolve each `DT_NEEDED` through `DT_RUNPATH`/`DT_RPATH` and the default
   path **in the workspace's view**: base → host base; `/usr/local` → the
   workspace's `.state/root/usr/local`; `/workspaces/A` → A's `work`; `~/work` →
   `work`; every open `O_RESOLVE_BENEATH` its folder; `[bundle] extra-libs` globs.
2. C SDK into the base: `libatrium` (`.so` and `.a`) and `libfresco`, headers under
   `/usr/include/atrium`, `atrium.pc`, `fresco.pc` in `/usr/libdata/pkgconfig`.
3. `atrium.app.mk`: `make run` (sends `WorkspaceRun` for `.`), `make bundle`
   (writes `bundle/` with the closure via a `portcullis workspace bundle` verb that
   reuses (1)).
4. `PKG_CONFIG_PATH` for uses links (dev-tools.md §4.3).

**Gate** `scripts/vm-run-closure-check.sh`: a C window app linking `libfresco` and
`libz` from `pkg` (M2) runs with `make run`; a library reached through a link out
of `work` is refused; an app linking workspace A's `.so` through a uses link runs.

**As built (2026-11-18).** Run resolves each library as the workspace's loader
would: `DT_RPATH` or `DT_RUNPATH` (with `$ORIGIN`), then `/lib`, `/usr/lib` and
`/usr/local/lib`. It looks through the workspace's view (`workspace::view`). Folders
the workspace can write are opened `O_RESOLVE_BENEATH`, so a link out of them is
refused before the press is spent. Base libraries keep their paths in the bundle.
The workspace's own go to `/usr/lib`, where the app's loader looks. The C SDK goes
into the base with `scripts/install-c-sdk.sh` (`libfresco.so.0` built natively,
`libatrium.so.0` cross-built, headers in `/usr/include/atrium`, `.pc` files,
`/usr/share/mk/atrium.app.mk`). `make run` calls `/atrium/bin/atrium-run`, which
asks the Terminal with OSC 7770 `run` the same way `pkg` does. The Terminal may now
send `WorkspaceRun` on the person's press. `make bundle` and `[bundle] extra-libs`
aren't done yet. Found on the way: a workspace had no loader hints, so programs from
pkg that link `/usr/local/lib` didn't start. pkg runs now end with `ldconfig`. The
gate `scripts/vm-run-closure-check.sh` passes 17 of 17 with the filter denying.

## 7. M5: workers (L)

**Code.**
- `portcullis/portcullis-toml/src/schema.rs`: `[[workers]]` (`name`, `entry`,
  `input`, `lifetime`, `memory`, `cpu`, `isolation`), `[roles]`, `[ipc]`; refuse
  capabilities on a worker; keep `deny_unknown_fields`; fix the stale comment.
- `portcullis/portcullisd`: install (and `dev.rs`) registers a uid per (person,
  app, worker) in the launch registry (`worker/<app>/<name>`); a new
  `WorkerCall { worker, … }` request alongside `ExecInstance` that resolves the
  worker from the **caller's own** installed manifest (by peer uid in the registry)
  and builds the rule that system bundles get from `[workers."id"]` today;
  `ExecInstance` keeps working only for ids with a system-policy rule (closes the
  gap noted in app-composition.md §9).
- New crate `atrium-worker` (SDK): core side `Workers::get::<T>(name)?.call(fd|msg)`,
  worker side `#[worker::main]`, framing with postcard and a bounds `check()`,
  timeouts, crash and limit to `Err`; modes `jail` (WorkerCall), `capsicum` (fork,
  `cap_enter`, rlimits, the Player's pattern), `inproc`. One `atrium-sandbox`
  module replaces the ~25 hand-written `cap_enter` blocks over time.
- Move **officina-convert** (`officina-convert/src/launch.rs`) and **mail-parse**
  (`mail/mail-engine/src/lib.rs:94`) onto it; both become workers of their apps.

**Gate** `scripts/vm-workers-check.sh`: a third-party test bundle with two workers
calls both; a worker can't read the core's overlay, open a socket or a file; a
crash, a timeout and a memory limit each come back as errors; the app can't call
another app's worker or start a system bundle; `vm-sheets`/Officina converter gates
and Mail's host tests pass with real jails.

**As built (2026-11-18).** The manifest has `[[workers]]`, `[roles]`, `[ipc]`,
`embeds` and `arca` (`portcullis-toml`, validated; the consent delta names `arca`
and new `embeds`). portcullisd answers `WorkerCall { worker }`: the caller is found
by peer uid in the launch registry, the worker in its **installed** manifest, and it
runs through the `ExecInstance` path with `portcullis_oneshot::as_worker` making the
manifest the worker's (its entry, no capability, only its `memory`), as a uid
registered under `worker:<app>:<name>` per person and persona. An app reaches it on
a second socket, `/atrium/sockets/workers/workers.sock`, mounted only into the jail
of an app that declares workers and answering nothing but `WorkerCall`; the app does
not get portcullisd's own socket. `ExecInstance` from an app with no system rule may
start only its own id or one under it. A Run copies each worker's entry and its
libraries into the bundle. The base syscall profile gained `getgroups` (jaild's
`verify_dropped` calls it; without it every one-shot failed in deny mode).
`sdk/atrium-worker`: `Workers::get()?.call(name, &req, file)`, `.session(name)`,
`.spawn_stdio(name)` for a worker with its own stream protocol, and
`atrium_worker::main!(handler)` (a macro in place of `#[worker::main]`); modes jail,
capsicum (a child that calls `cap_enter`), local (`ATRIUM_WORKERS=local`) and
in-process. Officina's converter is Write's `convert` worker and Mail's parser is
its `parse` worker, through `spawn_stdio`, falling back to a child process off
Atrium. Gate `vm-workers-check.sh` 19/19 in deny mode: the worker's own uid, stable
across calls; a file passed in; its error, a crash, a timeout as errors; its `memory`
as an rctl `memoryuse:sigkill` rule on its jail, and a worker past it killed and
reported as an error, the next call working; no network;
no nested calls; only the worker-call socket in the core; no jail left. Not done: a second worker and the
overlay check, `vm-sheets`/converter gates on real jails, Sheets and Slides (no
manifests yet), and the ~25 hand-written `cap_enter` blocks.

## 8. M6–M12 in brief

- **M6 (M).** `embeds = [roles]` capability: portcullis-jail mounts limend's socket
  (`capabilities.rs`, as `navigator-broker` does now) and limend checks the role
  against the caller's manifest; limend reads providers from installed manifests'
  `[roles] implements` instead of `LIMEN_DEFAULTS` (`limen/src/bin/limend.rs:31`)
  and portcullisd generates the `callers=["_limen"]` rule for them; `arca`
  capability scoped to the app's own items. Gate: a third-party app opens a picker
  and a `doc-viewer`; an app without `embeds` is refused; a third-party
  `share-target` provider is used.

  **As built (2026-11-18).** `embeds` mounts `/atrium/sockets/limen/` (portcullis-jail),
  and limend checks each request against the parent's installed manifest
  (`limen::manifests::Installed::may_embed`; refusal 15, `NOT_EMBEDDABLE`). A platform
  bundle (`org.atrium.*`, a prefix no third party is verified for, opifex.md §6.5)
  with no `embeds` key keeps every role, as before; every other app must name the
  role. A user role with no `LIMEN_DEFAULTS` entry goes to an installed app whose
  `[roles] implements` has it: a platform one first, else the only one (a dev build
  counts only for its own person); two third-party providers and no platform one give
  none rather than choosing for the person. portcullisd runs such a provider for
  limend (`_limen`, with `serves`) as a per-instance uid of the parent's person and
  persona, with no system rule; providers must still ask for no capability (and no
  `graphics`). `arca = true` mounts `/atrium/sockets/arca/`; arcad already scopes app
  ops to the caller's own items by its registry identity. Gate `vm-embeds-check.sh`
  13/13 in deny mode, without frescod (an embed that passes the checks ends at the
  embed connection): sockets present only when declared, roles outside `embeds`
  refused, a third-party `share-target` provider found from its manifest and started
  as its own uid for the parent. Not done: the gate with frescod and Scrinium (a real
  picker and doc-viewer on screen), Curia's choice of provider, Arca couriers for
  third-party requests.
- **M7 (L).** `cargo-atrium` (new, check, run, bundle, cross, fuzz) and
  `atrium-bundle` for C; templates (viewer, client, media, service; Rust and C);
  `#[derive(atrium::Message)]` with a schema file and `atrium-schema` generating C;
  the crate-graph lint with a curated parser list. Gate: each template builds, runs
  and fuzzes in a workspace; the lint flags `image` in a networked core and is silent
  for the viewer template.
- **M8 (M).** `registries` mode: a jailed `nuntius-egress` CONNECT proxy reusing
  `navigator-fetch`'s address policy; the workspace gets no route out, only the
  proxy; per-workspace host list in its record and the Workspaces app. Gate: cargo
  fetches crates; an unlisted host, a private address and the metadata address are
  refused; `outbound` workspaces unchanged.
  **As built (2026-11-18).** `nuntius-egress` (a `navigator-fetch` binary) accepts
  only `CONNECT host:443` for exactly listed names and refuses a name if any address
  it resolves to fails Nuntius's address policy (never the private-network grant).
  A `registries` workspace's jail is `loopback`. Before anything runs in it,
  portcullisd binds 127.0.0.1:3128 in the workspace's own stack (a helper,
  `portcullisd --bind-in-jail <jid>`, attaches and hands the socket back) and gives
  that listener to the proxy as stdin. The proxy runs as a one-shot jail of the system
  bundle `org.atrium.nuntius-egress`, one per running workspace, as `_egress` (65044)
  under the system worker rule (`scripts/nuntius-egress-bundle.sh`). The workspace gets
  `HTTPS_PROXY` and the related variables. Its host list is `hosts` in its record
  (`portcullis workspace set NAME --hosts a,b`, `WorkspaceSet.hosts`), defaulting to
  §5.2's list. Gate `scripts/vm-registries-check.sh`: 27/27 in deny mode. Not done:
  the Workspaces app's host list editor, and a `cargo fetch` (the gate fetches with
  `fetch(1)`, which uses the same proxy variables, because the cloud VM has no Rust
  layer).
- **M9 (M).** Needs Arca's SSH agent (Arca A5); this thread builds the workspace side:
  agent socket mounted per workspace, per-key grants in Curia, `git-credential-arca`,
  `c-dev`'s git. Gate: `git push` over SSH to a test server with the key in Arca;
  no key material in the workspace; a workspace without the grant is refused.
- **M10 (M).** `WorkspaceRun { debug: true }`: lldb-server as the dev app's parent in
  its jail, a socketpair back into the workspace, the frame tag; cores copied on
  request. Gate: a breakpoint in a Pergola dev app from `lldb` in the workspace;
  refused for an installed app.
- **M11 (L+).** Artifex v0 as a new app `artifex/` (org.atrium.artifex,
  `workspace-exec`, `embeds = ["terminal"]`): a code-grid text widget in Pergola
  (rope buffer, monospace fast path, multi-cursor) (P4); tree-sitter grammars in a
  syntax module (P6); workspace pick and tree; LSP client over `WorkspaceExec`;
  tasks from Cargo/make/Meson/CMake; terminal pane; git status by running git;
  **New project** from M7's templates; **composition panel** from the manifest and
  the launch registry. Gate `scripts/vm-artifex-check.sh` (on-screen): open Scratch,
  edit, a rust-analyzer diagnostic shows, Build, Run shows the amber dev window, the
  composition panel lists the template's core and worker.
  **As built (2026-11-17).** Built: `artifex/syntax` (tree-sitter 0.25 with eight MIT
  grammars, incremental reparse with a time budget) and `artifex/app` (Pergola's
  code grid, workspace chooser and tree, tabs, atomic saves that keep a workspace
  file's owner, LSP over `WorkspaceExec` with path mapping into the workspace, tasks
  from Cargo/Meson/CMake/make/a lone C file with a Problems panel, git status, the
  composition panel with `cargo atrium check`, New project from `cargo atrium new`,
  and the workspace's Terminal from a button). `vm-artifex-check.sh` passes on the
  cloud VM (pick, edit, save, build problems, jump to the error, the terminal), and
  with the c-dev and Rust layers installed (`CDEV=`, `RUST=`) clangd marks hello.c
  and rust-analyzer marks a Cargo project's main.rs in a Rust workspace. Language
  and debug servers run with their stderr dropped, since `WorkspaceExec` merges it
  into the output the protocol is framed on. Run (2026-11-18): a project with
  `atrium.toml` gets a Run task (`make run` on atrium.app.mk, `cargo atrium run`,
  Ctrl+R); its `atrium-run` asks Artifex as it asks a terminal (`ATRIUM_ASK=1`, the
  OSC 7770 request read from the task's output), Artifex shows "<workspace> asks to
  Run <folder>" with Run and Cancel, and the press on Run sends `WorkspaceRun`
  (portcullisd lets an IDE Run in a workspace it was given). In the gate a c-app
  project shows its core in the composition panel, and Run opens its window in the
  amber dev frame. libatrium now finds Fresco at the jail's socket when
  `ATRIUM_FRESCO_SOCKET` is unset, as a Run's dev app has it. M12's Artifex half is built too: the debugger panel on `lldb-dap` over
  `WorkspaceExec` (breakpoints in the gutter, stack, locals, stepping; end-to-end on
  Linux), and the fuzz panel running `cargo atrium fuzz` in the background, core
  crashes first with "Move into a worker", a crash opening at its line and replaying
  under the debugger (end-to-end on Linux against the real fuzzer).
- **M12 (L).** Artifex fuzz panel (background `cargo atrium fuzz` per worker and per
  core input surface, crashes in the core flagged with "Move into a worker"), DAP to
  `lldb-dap`, SDK registry (static sparse index), sysroot archives, the developer
  guide. Gate: a seeded parser in a template's core is found by the fuzz panel and
  the move action produces a worker that passes.

## 9. Questions for Shankar

| | Question | Recommendation |
|---|---|---|
| P1 | App consoles under deny mode: a separate console jail with the app's mounts and uid, or widen the app's own jail to `terminal` while a console is open | separate console jail; the app's jail keeps its class |
| P2 | Where the Rust layer is built | cloud thread (cross from Linux with `x.py dist`); the Mac only to verify |
| P3 | How to staff it | three threads: **platform** (M0, M2, M4, M5, M6, M8, M10), **terminal and Artifex** (M1, then M11, M12's Artifex part), **toolchains and SDK** (M3, M7, M12's SDK part); M9 coordinated with the Arca thread |
| P4 | Artifex's editor widget | a new code-grid widget in Pergola (monospace, rope, multi-cursor), sharing shaping and bidi with Officina, not a reuse of Write's paragraph engine |
| P5 | Package source for workspaces | `pkg.freebsd.org` now; an Atrium mirror pinned per release once releases exist |
| P6 | tree-sitter | vendor its C core and grammars (MIT; carve-out (a)), parsing in Artifex's jail; revisit a Rust parser later |

M0 can start as soon as P1 is answered; it doesn't depend on anything else.
