# Developer tools: editors, toolchains, builds and debugging

**Status:** accepted 2026-11-17 (Shankar: "yes, accepted", all §13 decisions
D1–D14 as recommended). Nothing here is built yet; T1 first.

**Asked by Shankar, 2026-11-17:** "we start a workspace, do we need a code
editor? vim/emacs style text ones or full fledged IDEs like xcode and visual
studio? lightweight ones like vscode or zed? the standard development flows of
doing -I <include_files> -l <library_files> in C still supported? rust specific
development for atrium? makefiles build scripts etc. first fully investigate,
think it through and come up with a proposal"

Related: [homes.md](homes.md) §5 (workspaces, W1–W9, all built 2026-11-15);
[atrium-devjail.md](atrium-devjail.md) (the May design workspaces grew from);
[artifex.md](artifex.md) (the May IDE sketch); [stoa.md](stoa.md) (consoles);
[arca.md](arca.md) §9 (SSH agent, git credentials); [atrium-bundle-format.md](atrium-bundle-format.md)
(the runtime closure); [toolkit-backends.md](toolkit-backends.md) (winit, SDL);
[jail-sysfil.md](jail-sysfil.md) (syscall classes); [kernel-hardening.md](kernel-hardening.md)
§4; [nuntius.md](nuntius.md); [LICENSING-POLICY.md](../LICENSING-POLICY.md);
[LANGUAGE-POLICY.md](../LANGUAGE-POLICY.md).

---

## 0. The answers in short

| Question | Answer |
|---|---|
| Do we need a code editor? | Yes, in two steps. First, terminal editors (vi, Helix, Neovim, Vim, Emacs, Kakoune, micro) running inside workspaces, which costs almost nothing once the Terminal can draw them (§3.3). Then **Artifex**, our own native code editor, a Pergola app in the Zed/VS Code-lite class that edits a picked workspace and runs everything (builds, language servers, debugger) inside it through `workspace-exec` (§3.4). |
| Full IDEs (Xcode, Visual Studio)? | Not as ports: they are proprietary and tied to their OS. Artifex grows toward that class in phases (debugger, refactoring, profiler), but we don't start there. |
| VS Code or Zed? | Neither can run on Atrium. VS Code is Electron (Chromium on X11 or Wayland), and Atrium has neither; its remote server ships only for Linux, macOS and Windows. Zed's editor is GPL-3.0 and its UI layer would need a Fresco platform backend. Lapce (Apache-2.0) and lite-xl (MIT) become possible third-party ports once the winit and SDL toolkit backends exist (§3.2). |
| Does `cc -I… -L… -l…` still work? | Yes, unchanged, inside a workspace (§4). The base headers and libraries are there read-only, the workspace's own `/usr/local` is writable, linked workspaces appear at `/workspaces/<name>`. What changes is running a *windowed* result: it runs as its own dev app (W5), so its libraries must be carried into a bundle, which today only works for base libraries (§4.5). |
| Makefiles and build scripts? | `make` is FreeBSD's bmake from the base system; GNU make, CMake, Meson, Ninja, autotools and pkgconf come as packages in the workspace (§4.4). `build.rs`, `Makefile` recipes and `npm` hooks run inside the workspace's jail, which is exactly where untrusted build code should run (§9). |
| Rust? | A signed Rust toolchain layer for both arches (aarch64 has no official rustup builds), crates fetched by cargo from the workspace, an **Atrium SDK** (published crates, C headers, a sysroot for cross builds) and a `cargo atrium` subcommand for new, run and bundle (§5). |
| Debuggers? | lldb is already in the base system. Workspaces need a `develop` syscall class with `ptrace` and per-jail unprivileged debugging so a workspace can debug its own processes; a dev app can be **run under the debugger** through portcullisd (§7). |
| Composing an app out of jailed parts, as the Navigator is? | Its own proposal, [app-composition.md](app-composition.md): system services instead of libraries, `[[workers]]` in the manifest called through the SDK like functions, typed messages, templates and a lint that finds parsers in the wrong process. |
| Version control? | git as a package in the workspace (a tool, nothing links it); keys stay in Arca behind an SSH agent socket (§8). |

## 1. What exists today

Built (homes.md §8, all 2026-11-15):

- **Workspaces** (W3): a persistent jail per workspace (`ws_<person>_<name>`),
  its own uid, the base system read-only (`/bin`, `/lib`, `/usr/{bin,include,lib,
  libexec,share,…}`), its own writable `/etc`, `/tmp`, `/var`, `/usr/local` and
  `~/work`. Network `outbound` (a routed epair) or `none`. `cc hello.c && ./a.out`
  works in Scratch.
- **Uses links** (W3), **Duplicate** (W8), **object cache by group** (W9).
- **Terminal** (W7): a regular app; alone it is an empty jail; a switcher moves a
  tab into a workspace or an app's console. Kept by Stoa (W2).
- **Run** (W5): `portcullis workspace run` / the Workspaces app installs a project's
  `atrium.toml` + entry as `dev.<person>.<ws>.<id>`, unsigned-but-dev-signed, amber
  frame, no restricted capabilities.
- **`workspace-exec`** (W6): an IDE app gets picked workspaces at `/work/<name>` and
  runs commands in them as the workspace's uid (`WorkspaceExec { name, argv, cwd }`,
  answered with a process descriptor, stdin and output). Only a probe client exists
  (`atrium-term-socket` example `ide_probe`); no IDE.

Designed, older: [atrium-devjail.md](atrium-devjail.md) (May) reached dev jails
over **SSH** from a jailed VS Code; homes.md replaced that with consoles and
`workspace-exec`, which need no sshd, no keys and no network between jails.
[artifex.md](artifex.md) (May) sketched the IDE with language servers in sibling
jails over a workspace descriptor; that predates workspaces (§3.4 updates it).

Gaps found reading the code (2026-11-17):

1. **Only C and the shell are offered.** `TOOLCHAINS` in
   `portcullisd/src/workspace.rs` lists Rust, Python, Go and Node as unavailable
   until atrium-pkg layers exist. And `pkg install` cannot work in a workspace at
   all: commands run as the workspace's uid, `/usr/local` is that uid's, but `pkg`
   insists on root and `/var/db/pkg` is root's.
2. **The workspace's syscall class is wrong for a build box.** Its manifest is
   generated with only `network`, so `sysfil_class` makes it `networked` (or `app`
   with no network). Neither class has `execve`. Shells and compilers work today
   only because sysfil runs in log mode; switching to deny would break every
   workspace.
3. **The Terminal can't draw full-screen programs.** `stoa-term` (the emulator
   behind Terminal) defers scroll regions (DECSTBM), the alternate screen and the
   DEC private modes (mouse, bracketed paste). vi, Vim, Helix, `less` and `top` need
   them.
4. **Run carries base libraries only.** A dev build linked against something in
   the workspace's `/usr/local` or a linked workspace doesn't start (homes.md §8 step 5:
   "a library from the workspace's `/usr/local` isn't carried yet").
5. **No debugging anywhere.** `security.bsd.unprivileged_proc_debug=0` system-wide
   and `ptrace`, `ktrace` and `procctl` are in no class.
6. **No git, no credentials.** FreeBSD's base has no git; Arca's SSH agent
   (arca.md §9, step A5) isn't built.
7. **No SDK.** The Atrium crates are path dependencies with `publish = false`; only
   two headers live in `include/atrium`; `libatrium` and `libfresco` aren't
   installed where a workspace could link them; `.cargo/config.toml` hardcodes the
   Mac's sysroot path for cross builds.

## 2. Principles

1. **Tools run in the workspace; editors are just UI.** Compilers, language
   servers, debuggers, test runners and terminal editors run in the workspace's
   jail, as its uid, with its network. An editor app holds no toolchain and
   executes nothing itself; it asks the workspace (`workspace-exec`). So there is
   one place where a project's untrusted code (build scripts, macros, tests) runs,
   and it is the place built to hold it.
2. **The base system stays small and permissive.** Dev tools come as **layers**
   (read-only, signed, content-addressed) and **packages** (installed into one
   workspace). LICENSING-POLICY.md already says tools that don't ship in the
   runtime can carry any license that lets us use them; nothing links against git
   or GNU make. The rule this proposal adds: a GPL tool may sit in a dev layer or a
   workspace, never in the base system and never in an app bundle.
3. **Port when the license and the display allow it; build when Atrium's model
   gives something better.** A terminal editor is a port. A sandboxed IDE whose
   language servers run in the project's jail is something no existing editor
   does, so we build it.
4. **The C ABI and the usual Unix flows keep working.** A C programmer should
   find `cc`, `make`, `/usr/include`, `-lfoo` and `pkg-config` where FreeBSD puts
   them.

## 3. Editors

### 3.1 The candidates

| Editor | Kind | License | On Atrium | Verdict |
|---|---|---|---|---|
| **nvi** (`vi`) | terminal, modal | BSD | in the base system and the console toolbox | ships today |
| **ee** | terminal, simple | BSD | base system | ships today |
| **Helix** | terminal, modal, LSP and tree-sitter built in | MPL-2.0 | FreeBSD package, in the workspace | **recommended default** for code in a terminal |
| **Neovim** | terminal, modal, LSP built in | Apache-2.0 (Vim-derived parts under the Vim license) | FreeBSD package | yes |
| **Vim** | terminal, modal | Vim license (permissive, charityware) | FreeBSD package | yes |
| **Kakoune** | terminal, modal | Unlicense | FreeBSD package | yes |
| **micro** | terminal, non-modal, mouse | MIT | FreeBSD package | yes; the friendly one for people who don't want modal editing |
| **Emacs** (`emacs-nox`) | terminal | GPL-3.0 | FreeBSD package, workspace only | yes as a tool; GUI Emacs needs X11 or GTK, so no |
| **VS Code** | GUI, Electron | Code-OSS MIT, product proprietary | needs Chromium on X11 or Wayland; Remote-SSH's server is built for Linux, macOS and Windows only | **no** |
| **Zed** | GUI, GPU | editor GPL-3.0 (gpui Apache-2.0, server AGPL) | gpui would need a Fresco + Vulkan platform layer; remote server Linux and macOS only | **no**; a third party could port it later, we wouldn't ship it |
| **Lapce** | GUI, Rust | Apache-2.0 | floem on winit + wgpu: runs once the winit backend exists (toolkit-backends.md §5, step 1) | **later**, as a source port |
| **lite-xl** | GUI, C + Lua | MIT | SDL2: runs once the SDL backend exists | **later**, as a source port |
| Xcode, Visual Studio, JetBrains | full IDEs | proprietary | tied to their OS or to a Linux backend | no |

Remote editing from another computer (the Mac) also mostly fails: VS Code
Remote, Zed remote and JetBrains Gateway all need a server binary that isn't
built for FreeBSD. What does work from the Mac is a terminal: Stoa's remote
client into a workspace console, with a terminal editor there.

### 3.2 Why not port VS Code or Zed

- **VS Code** is Chromium plus Node. Atrium has no X11 or Wayland and doesn't want
  them (the Fresco charter), and a Chromium port is the whole legacy-web problem the
  Navigator deliberately avoids. Its value is its extension marketplace, which
  Microsoft licenses for its own builds only; Open VSX extensions often ship Linux
  binaries.
- **Zed** is the closest in spirit (native, fast, Rust), but its editor is GPL-3.0
  and its UI framework talks to Metal, Vulkan, Wayland and X11 through its own
  platform layer. Porting means writing a gpui platform for Fresco and keeping up
  with a fast-moving upstream for an app we couldn't ship as part of Atrium.
- What we would take from them is ideas: Zed's speed and multi-buffer editing, VS
  Code's task and debug model, LSP and DAP as the protocols.

### 3.3 Step one: terminal editors (almost free)

Install them in a workspace with `pkg` (§6) or get them from the C and Rust dev
layers (§6.2), and run them in Terminal. They see the project, the compilers and
the language servers because they run where those are. Helix and Neovim talk to
rust-analyzer and clangd with no setup.

What Atrium must do for this to work:

- **Finish the Terminal's emulator** (gap 3): scroll regions, alternate screen,
  tab stops, cursor save and restore, insert and delete line and character, DEC
  private modes (application cursor keys, mouse 1000/1002/1006, bracketed paste
  2004, focus events 1004), OSC 0/2 titles, OSC 52 clipboard writes (through the
  Terminal's own `clipboard` capability, never read back silently), and
  `TERM=xterm-256color` with matching behaviour. Gate: vi, Vim, Helix, Neovim,
  `less`, `top` and `htop` render correctly in scripted screen dumps.
- **`EDITOR=vi`** in consoles by default, set per workspace in its settings.
- **Clipboard** between the Terminal and other apps already goes through the
  Terminal's `clipboard` capability; OSC 52 makes editors use it.

### 3.4 Step two: Artifex, a native code editor

A Pergola app in the lightweight class (Zed, Sublime, VS Code without the
marketplace). It reuses what we already have: Pergola's text editing, fresco-text
shaping, Officina's bidi lines, the Terminal for its terminal pane, and
`workspace-exec` for everything that runs code.

**Artifex v0 (the first useful version):**

- Pick one or more workspaces (W6's press); a folder tree of `/work/<name>`.
- Tabs, splits, a fast monospace grid path, multi-cursor, find and replace,
  search across files (`rg` run in the workspace, or a built-in walker over
  `/work`).
- Syntax by **tree-sitter** (MIT; its C core is upstream code, allowed by
  LANGUAGE-POLICY.md's carve-out (a)) for Rust, C, C++, TOML, Markdown, shell,
  Python, JSON first.
- **LSP**: Artifex starts `rust-analyzer` or `clangd` with `WorkspaceExec` and
  speaks JSON-RPC over the pipes it gets back. The server runs as the workspace,
  sees exactly what the build sees, and is killed when the descriptor closes. This
  replaces artifex.md §3's sibling jails: a language server needs the project's
  toolchain, its dependencies and its build output, which are all in the workspace.
- **Tasks**: Build, Test, Run from `Cargo.toml`, a `Makefile`, `meson.build` or
  `CMakeLists.txt`, run with `WorkspaceExec`; errors parsed into the problems list.
  Run of a windowed app goes through W5 (`WorkspaceRun`).
- **Terminal pane**: a Terminal console on the same workspace, as a Limen embed
  (artifex.md §5).
- **git**: status in the tree and gutter, diff view, stage and commit, by running
  `git` in the workspace (§8). Reading status with gitoxide (MIT/Apache) in
  Artifex's own process over `/work` is faster and is fine for read-only views.

**Later phases** (artifex.md §11, re-ordered): the debugger (DAP to `lldb-dap` in
the workspace, §7); refactoring and code actions; the extension API as a Limen
`editor-extension` role; AI assist with keys in Arca; a profiler view.

**What Artifex never gets:** a shell of its own, the person's home, network
(unless an extension declares one), or anything outside the workspaces picked in
it. A hostile repository can reach Artifex only through the files it reads, so
its parsers (tree-sitter, Markdown preview, image preview) are written for
hostile input and the previewers run as Limen embeds.

**Relation to `atrium-edit`.** The foundation text editor stays a plain text
editor (notes, config files, one file at a time, no workspace). Artifex is for
projects. They share the buffer and text code.

## 4. C and C++ in a workspace

### 4.1 What works today

Inside any workspace, from a Terminal console:

```
$ cc -O2 -Wall -I/usr/local/include -c foo.c
$ cc -o foo foo.o -L/usr/local/lib -lz -lm
$ ./foo
$ make            # FreeBSD's bmake
$ ar rcs libbar.a bar.o && cc -shared -o libbar.so bar.o
```

clang, lld, `ar`, `nm`, `objdump`, `lldb` and bmake are FreeBSD's own and come
from the read-only base. Nothing about `-I`, `-L`, `-l`, `-Wl,-rpath`, static or
shared linking changes. A program that doesn't open a window runs right there in
the workspace, with `LD_LIBRARY_PATH` and rpaths behaving as usual.

### 4.2 Where things are

| Path in the workspace | What | Writable |
|---|---|---|
| `/usr/include`, `/usr/lib`, `/usr/libdata`, `/usr/share/mk` | the base system, same as the host | no |
| `/usr/local/{bin,include,lib,libdata/pkgconfig}` | this workspace's packages and `make install`s | yes (its uid) |
| `/atrium/toolchains/<layer>` | dev layers (§6.2), on `PATH` | no |
| `~/work` | the sources, also shown in Scrinium | yes |
| `/workspaces/<A>` | workspace A's `work`, when this one uses A | no (yes if linked read-write) |
| `/cache/objects` | ccache or sccache objects, when in a group (W9) | yes |
| `/usr/include/atrium`, `/usr/lib/lib{atrium,fresco}.so`, `/usr/libdata/pkgconfig/{atrium,fresco}.pc` | the Atrium C SDK (§5.4), proposed in the base system | no |

`PREFIX=/usr/local` is the default for `make install`, CMake and Meson, and lands
in the workspace's own tree. There is no `sudo`: the workspace's uid owns
`/usr/local`, which is what `make install` needs.

### 4.3 Building against another workspace

B uses A (homes.md §5.2): `cc -I/workspaces/A/include -L/workspaces/A/build
-lfoo`. To make pkg-config find A's libraries, portcullisd adds
`/workspaces/<A>/.atrium/prefix/lib/pkgconfig` to `PKG_CONFIG_PATH` for each
linked workspace that has one, so a project that installs into
`~/work/.atrium/prefix` (`--prefix` or `PREFIX=`) is found by its users without
copying.

### 4.4 Build systems

| Tool | License | Where |
|---|---|---|
| bmake (`make`), `bsd.prog.mk` / `bsd.lib.mk` | BSD | base |
| pkgconf (`pkg-config`) | ISC-style | C dev layer (FreeBSD's base has none) |
| CMake | BSD-3-Clause | C dev layer |
| Meson + Ninja | Apache-2.0 | C dev layer |
| GNU make (`gmake`) | GPL-3.0 | C dev layer (a tool; D6) |
| autoconf, automake, libtool | GPL with exceptions for generated files | package, on demand |
| ccache | GPL-3.0 | package; W9's cache already sets `CCACHE_DIR` |
| `compile_commands.json` | | from CMake and Meson; for make, `bear` (GPL-3.0, package) |

A small `atrium.app.mk` in `/usr/share/mk` (in the style of `bsd.prog.mk`) gives
plain Makefiles `make run` (asks for Run, W5) and `make bundle` (§5.5), so a C app
needs no new build system.

### 4.5 Running what you built: the closure

A windowed program runs as a dev app in its own jail (W5), whose root is the
bundle: there is no shared `/usr/lib` there (atrium-bundle-format.md §1). So Run
copies the program's libraries into the bundle. Today it does this for base
libraries only (gap 4). Proposed:

- portcullisd reads `DT_NEEDED` and `DT_RUNPATH` from the entry (as now: parsed,
  never run) and resolves each library **in the workspace's view**: the base system
  from the host's base; `/usr/local/...` from the workspace's root; `/workspaces/A/...`
  from A's `work`; `~/work/...` from `work`. Every open is `O_RESOLVE_BENEATH` the
  folder it comes from, so a link out is refused, as for the entry today.
- Each library lands in the bundle's `lib/`, where rtld finds it by its default
  path; nothing is rewritten.
- Libraries a program `dlopen`s by name can't be found by reading the ELF; the
  manifest lists them (`[bundle] extra-libs = ["lib/plugins/*.so"]`).
- `make bundle` and `cargo atrium bundle` do the same resolution to produce an
  installable bundle for Opifex (§5.5).

### 4.6 What behaves differently from a normal FreeBSD box

- **No root.** `pkg install` goes through §6; `sudo` doesn't exist.
- **No kernel work on the host.** Building a kernel or a kmod works (with a source
  tree in `work` and `SYSDIR`); loading one is administration (homes.md §6), never
  from a workspace.
- **No setuid, no mounts, no raw sockets, no `/dev` beyond the jail's ruleset.**
- **W^X.** `kern.elf64.allow_wx=0` system-wide. JIT runtimes that map pages
  writable and executable at once fail unless they flip RW to RX. D11 asks whether
  workspaces may honour FreeBSD's per-binary `wxneeded` note.
- **Sanitizers.** ASan, UBSan and TSan work with FreeBSD's clang; ASan reserves a
  large shadow mapping, so the workspace's address-space limit must allow it.


### 4.7 Running a plain Unix binary in the workspace (Shankar, 2026-11-17)

Shankar asked: if someone builds a standard Unix binary, it is already in a
workspace, so does it just run inside the workspace's sandbox, which has a network
and other reach?

Yes, and that is deliberate: running what you build is the edit-build-test loop
(tests, CLI tools, servers on loopback, the debugger). It runs as the workspace's
uid with exactly the workspace's reach, which is everything the project's own build
scripts already have:

| It can | It can't |
|---|---|
| read and write this workspace's `work` and root, read linked workspaces, use the group object cache | reach the person's home, other workspaces, apps' data, Arca's items |
| use the workspace's network (`none`, `registries` or `outbound`) | open a window (workspaces have no display, H8) |
| ask the Arca agent to sign, for keys granted to this workspace (T7) | get a private key, or sign silently where the key asks for a touch |

So the boundary is the workspace, not the program, and the guidance follows from
that:

- **Running your own code there is fine; running untrusted binaries there means
  trusting them with that workspace.** One workspace per untrusted project (§9).
- **Key grants to a workspace are opt-in and per key**, and pushing keys default to
  "touch per use" or "per session" so a test binary can't push or sign behind the
  developer's back (T7).
- **The network is the workspace's setting**; a project that wants its tests
  offline uses `none` or `registries`.
- **Using a tool for real work is not running it in the workspace.** A windowed
  program becomes a dev app (Run, W5) or an installed app. A command-line tool the
  person wants to use on their own files should likewise become a bundle: a
  **command bundle** (an `atrium.toml` with `kind = "command"`), installed through
  Opifex, which Terminal runs in its own jail with only its declared capabilities
  and with files handed in by the picker or by an app console. Its design is left
  for a later step (after T5); until then CLI tools are used inside workspaces.

## 5. Rust

### 5.1 The toolchain

| Target | Upstream tier | Native toolchain on Atrium |
|---|---|---|
| `x86_64-unknown-freebsd` | 2 with host tools | rustup works in a workspace |
| `aarch64-unknown-freebsd` | 3 | no rustup builds; FreeBSD's `lang/rust` package (stable, a few weeks behind) or one we build |

Atrium's own tree pins nightly with `rust-src` for `build-std`, because it cross
builds to a tier 3 target from the Mac.

**Proposed (D3):** Atrium builds its own **Rust layer** from upstream source for
both arches (stable, plus the nightly the tree pins), cross-built from Linux in CI,
signed with the platform key and published through Opifex as a layer: `rustc`,
`cargo`, prebuilt `std` for both FreeBSD targets (so no `build-std` on the target),
`rust-src`, clippy, rustfmt, rust-analyzer, sccache and `cargo-atrium`. A workspace
pins a version in `.atrium/devjail.toml` (`rust_version`); layers are shared and
deduplicated by Tessera. rustup stays allowed in an amd64 workspace for people who
want it. Longer term, we offer to maintain `aarch64-unknown-freebsd` upstream and
work toward tier 2, which needs named target maintainers and CI on real hardware.

### 5.2 Crates and the network

Cargo talks to crates.io itself, from the workspace, over the workspace's own
network. It doesn't go through Nuntius: Nuntius gives apps bytes through an API,
while cargo, git, pkg, pip and npm speak HTTPS (and SSH) themselves. What Nuntius
does have is the right address policy, so the proposal (D5) is a third network
mode for workspaces next to `outbound` and `none`:

- **`registries`**: no route out, only `HTTPS_PROXY` to a jailed forward proxy
  built from Nuntius's fetcher core (CONNECT only, TLS stays end to end), which
  allows the hosts the workspace lists and refuses private and link-local
  addresses and the metadata endpoint. Default list for a Rust workspace:
  `index.crates.io`, `static.crates.io`, `crates.io`, `github.com`,
  `codeload.github.com`, `objects.githubusercontent.com`, `static.rust-lang.org`,
  `pkg.freebsd.org`. Added to in the workspace's settings, never from inside.
- `outbound` stays the default for new workspaces (it is what people expect);
  `registries` is offered for projects that want supply-chain hygiene and becomes
  the default for workspaces made from a cloned repository's manifest if that
  manifest lists hosts.

Per-workspace downloads, no shared download cache (H12, unchanged). `Cargo.lock`
and `cargo vendor` work as anywhere.

### 5.3 Build scripts and proc macros

`build.rs` and procedural macros run arbitrary code at build time, and so does
rust-analyzer when it expands macros. In Atrium they run in the workspace's jail:
they can read that workspace and its read-only links, use its network, and write
its files. They can't reach the person's home, Arca, other workspaces, the
display or the host. This is the main security gain over a normal laptop (§9).

### 5.4 The Atrium SDK

Today an app is a crate in this repository with path dependencies. For anyone
else, and for building Atrium apps in a workspace, we publish an SDK:

- **Rust crates**: `pergola`, `fresco-client`, `fresco-protocol`, `libatrium`, the
  portcullis client crates an app needs, `atrium-fonts`, each versioned together as
  one SDK version, published to an **Atrium crate registry** (cargo's sparse
  registry protocol, static files, `registry.atrium.dev`) and carried inside the
  Rust layer as a local copy for offline workspaces. License checks (`cargo deny`,
  LICENSING-POLICY.md) run on the SDK itself.
- **C SDK**: `atrium.h`, `fresco.h` and the ABI headers under `/usr/include/atrium`,
  `libatrium` and `libfresco` as shared and static libraries in `/usr/lib`, and
  `.pc` files in `/usr/libdata/pkgconfig`, in the base system (they are runtime
  libraries anyway, permissive). `pkg-config --cflags --libs atrium`.
- **Sysroot** for cross builds from macOS and Linux: one archive per SDK version and
  arch (`lib`, `usr/lib`, `usr/include`, the CRT objects) plus a `.cargo/config.toml`
  fragment that takes the sysroot path from an environment variable. This replaces
  the Mac paths in today's `.cargo/config.toml` and the ISO-extraction recipe the
  cloud threads use.
- **Templates**: `cargo atrium new --app` (a Pergola window, an `atrium.toml` with
  no capabilities) and `atrium-new --c` (a C window on `libatrium`, a Makefile using
  `atrium.app.mk`).

### 5.5 `cargo atrium`

A cargo subcommand (and an `atrium-bundle` CLI for C):

| Command | Does |
|---|---|
| `cargo atrium new [--app\|--service]` | a project from the SDK templates |
| `cargo atrium check` | lints `atrium.toml`: unknown capabilities, restricted ones, what the consent prompt will say |
| `cargo atrium run` | builds, then asks portcullisd for Run (W5); from a console the person's own CLI path needs no press, as `portcullis workspace run` today |
| `cargo atrium bundle [--sign <key>]` | builds release, resolves the closure (§4.5), writes the bundle; signing with a publisher key held in Arca |
| `cargo atrium cross <target>` | builds for the other arch with the layer's std and the SDK sysroot |

### 5.6 Building Atrium itself

A goal, not a first step: the atrium repository builds in an Atrium workspace on
the Mac VM and on the R9700 box, natively, with the Rust layer's prebuilt `std`.
That proves the toolchain story on the hardest project we have. The Mac and cloud
cross builds stay the daily path until then.

## 6. Getting tools into a workspace

### 6.1 `pkg` in a workspace (gap 1)

Options:

- **(a) root inside the workspace's jail, for `pkg` only.** The person runs `pkg
  install helix` in a console; a small `pkg` wrapper on the workspace's `PATH` asks
  portcullisd (`WorkspaceAdmin { name, argv }`, a press, like Run), which runs the
  real `pkg` as root **inside the workspace's jail**: the base is mounted read-only,
  the jail has no `allow.mount`, no raw sockets, no `chflags`, its own `/var/db/pkg`,
  and a `pkg` syscall class. Package scripts run there too, as jail root, harmless
  to the host apart from kernel bugs reachable from jail root.
- **(b) layers only.** Atrium builds every tool people need as a signed layer;
  `pkg` never runs. Safer, but we would rebuild the ports tree.
- **(c) host `pkg -r` into the workspace's root.** Package scripts would run as host
  root. Rejected.

**Recommended (D4): (a) for anything, plus (b) for the common toolchains.** Packages
come from `pkg.freebsd.org` for the base's ABI (FreeBSD:16) until Atrium has
releases; then from an Atrium mirror pinned per release, so the base's library
versions and the packages agree.

### 6.2 Dev layers

Read-only, signed, Tessera-deduplicated, mounted at `/atrium/toolchains/<name>`
and put on `PATH` by the workspace's toolchain choice:

| Layer | Contents |
|---|---|
| `c-dev` | pkgconf, CMake, Meson, Ninja, gmake, git, clangd and clang-format (the base has clang but not its tools), `bear`, Helix |
| `rust` | §5.1 |
| `python`, `go`, `node` | from FreeBSD packages, repackaged per version |

New workspace's toolchain list then turns Rust, Python, Go and Node on.

## 7. Debugging and profiling

### 7.1 Debugging inside a workspace

A test binary or CLI tool started in the workspace is a process of the workspace's
uid in the workspace's jail. Debugging it needs:

- a **`develop` syscall class** for workspaces: `terminal` (exec, ttys) plus
  `ptrace`, `ktrace` and `procctl`, and the networked set when the workspace has a
  network. This also fixes gap 2 before sysfil goes to deny mode.
- **`allow.unprivileged_proc_debug`** on workspace jails only; the host keeps
  `security.bsd.unprivileged_proc_debug=0`. With `see_other_uids=0` and the jail
  boundary, a workspace can trace only its own processes.

Then `lldb ./target/debug/foo` works (lldb is in the base system); `gdb` (GPL-3.0)
is a package for people who want it.

### 7.2 Debugging a dev app

A Run build is another uid in another jail, so the workspace can't trace it. **Run
under debugger** (`WorkspaceRun { …, debug: true }`, same press):

1. portcullisd starts the dev app's jail with `lldb-server gdbserver` (from the
   base, mounted read-only) as the parent of the app, and `ptrace` allowed in that
   jail only;
2. lldb-server's end of a socket pair is handed back into the workspace, where
   `lldb` (or Artifex through `lldb-dap`) connects with `gdb-remote`; no network
   port is opened;
3. the app's frame says "dev build, debugging".

Only `dev.` apps. Installed apps, signed by a publisher, are never debuggable this
way (D9). A dev app that crashes writes a core into its own overlay; Run copies it
into `~/work/.atrium/cores/` when asked.

### 7.3 Profiling

DTrace is root's. Later (not in this proposal's steps): portcullisd runs a fixed
sampling script (`profile-997`, user stacks) against a dev app's pid on request and
hands back folded stacks for a flame graph in Artifex. Until then: `ktrace` in the
workspace, and timing builds.

## 8. Version control and credentials

- **git** in the `c-dev` layer and as a package (GPL-2.0; a tool, nothing links it).
  **gitoxide** (MIT/Apache) inside Artifex for read-only views. **Jujutsu**
  (Apache-2.0) as a package for people who like it.
- **SSH keys stay in Arca** (arca.md §9, A5): portcullisd mounts an agent socket into
  the workspace and sets `SSH_AUTH_SOCK`; Arca signs with keys the person granted to
  that workspace (per key, per workspace, in Curia). The private key never enters the
  workspace. `git push` over SSH and SSH commit signing (`gpg.format=ssh`) both use
  it. Each signature is logged; a key can require a touch (Sigillum) per use.
- **HTTPS tokens** (a GitHub token, `cargo publish`): a `git-credential-arca` helper
  and cargo's credential-provider protocol ask Arca, which releases the token to that
  workspace under a grant (an app-held item, arca.md §4.3). Weaker than a key, so SSH
  is the recommended way to push.
- **Personas**: a Work workspace sees only Work keys and tokens (personas.md).

## 9. Security model for development

What a developer machine usually trusts, and what Atrium does instead:

| Threat | Usual machine | Atrium |
|---|---|---|
| A malicious dependency (`build.rs`, proc macro, `npm` postinstall, a Makefile) | runs as you: your home, SSH keys, browser cookies, every other project | runs in one workspace: its files, its read-only links, its network; no home, no Arca, no display, no host |
| A malicious repository opened in the editor (tasks, modelines, language server config) | the editor runs it as you | the editor runs nothing; tasks and language servers run in the workspace; the editor parses files as hostile input |
| Key theft | `~/.ssh/id_*` readable by any process | keys in Arca, used through an agent with per-workspace grants and a log |
| Exfiltration of the project | anything can upload anything | per-workspace network: `none`, `registries` (allow-list proxy), or `outbound` |
| A dev build pretending to be a real app | nothing marks it | amber frame "dev build", no restricted capabilities, grants asked per run (W5) |
| Cache poisoning across projects | shared `~/.cargo`, ccache | no shared download cache; object cache only within an opted-in uses group (W9) |
| Persistence after a compromise | shell rc files, launch agents, cron | none outside the workspace; delete or duplicate-from-clean the workspace |

What it does not solve:

- A compromised workspace controls its own sources: it can change what you build,
  Run and commit there, and through a uses link or an object-cache group it taints
  the workspaces that trust its output. Advice in the Workspaces app: one workspace
  per untrusted project.
- The `develop` class and jail root for `pkg` widen the kernel surface a workspace
  can reach compared to an app jail. Kernel bugs are the residual risk, as for any
  jail.
- An editor's file parsers see hostile input. Artifex keeps them in its own jail,
  which holds only the picked workspaces.
- AI coding agents and other terminal tools run with the workspace's network and
  files: a good box for them, no more.

## 10. Old documents this changes

- atrium-devjail.md: the SSH access model (sshd in every dev jail, keys,
  `/etc/hosts`, VS Code Remote) is superseded by consoles and `workspace-exec`;
  `.atrium/devjail.toml` stays as the reproducible description, with `[ide.vscode]`
  dropped and `[network] mode = "registries"` added. Mark it so once accepted.
- artifex.md: §3 (sibling-jail language servers), §4.1 (debug capability) and §9
  (Scrinium workspace) are replaced by §3.4 and §7 here; the bring-up order becomes
  §12's.

## 11. Others considered

- **A general host shell for developers.** Rejected again (homes.md H6): every
  build tool would run as the person.
- **A Linux userland in a workspace** (to run VS Code Server or Linux toolchains).
  Hospes is a foreign-library host for Widevine, not a Linux ABI; the toolkit
  backends doc already rejects running Linux binaries for apps. No.
- **Building our own terminal editor.** Helix and friends are good and permissive
  enough; our effort goes into the Terminal and Artifex.
- **Language servers in their own jails beside the workspace.** They need the
  toolchain, the dependencies and the build output, which would all have to be
  mounted in; it's the workspace again with more mounts.

## 12. Build steps

| Step | What | Gate |
|---|---|---|
| **T1** | `develop` syscall class for workspaces, per-jail unprivileged debugging | with sysfil in deny mode, `cc`, `make`, a shell and `lldb` on a test binary work in Scratch; an app jail still can't `ptrace` |
| **T2** | Terminal emulator completeness (§3.3) | scripted screen dumps of vi, Vim, Helix, Neovim, `less`, `top`; mouse and bracketed paste in Helix |
| **T3** | `pkg` in a workspace (`WorkspaceAdmin`, §6.1) | `pkg install helix` asks a press, lands in that workspace only; package scripts can't touch the base or host |
| **T4** | `c-dev` and `rust` layers, both arches, signed; New workspace offers Rust | `cargo new && cargo test` in a Rust workspace; rust-analyzer answers through `WorkspaceExec`; clangd on a C project |
| **T5** | Run closure from `/usr/local` and uses links; C SDK in the base; `atrium.app.mk`; `cargo atrium` new, check, run, bundle | a C app linking `libfresco` and a library from `pkg`, and a Pergola app, both start with `make run` / `cargo atrium run`; a link out of `work` is refused |
| **T6** | `registries` network mode (§5.2) | crates.io fetches, an unlisted host and a private address are refused |
| **T7** | Arca SSH agent and git credentials in workspaces (needs Arca A5) | `git push` over SSH with the key in Arca; the key never appears in the workspace; another workspace without the grant is refused |
| **T8** | Run under debugger, cores (§7.2) | breakpoint in a Pergola dev app from `lldb` in the workspace; refused for an installed app |
| **T9** | Artifex v0 (§3.4) | open a picked workspace, edit, diagnostics from rust-analyzer, Build and Run, terminal pane, git status |
| **T10** | Artifex debugger (DAP), SDK registry and sysroot archives published | step through a dev app in Artifex; build an app from macOS with only the published SDK |

T1 is small and should land before sysfil goes to deny mode, whatever else is
decided. T2 and T3 together already give a usable terminal development setup.

## 13. Decisions for Shankar

| | Decision | Recommendation |
|---|---|---|
| D1 | Do we build our own code editor? | Yes: Artifex, a native lightweight IDE (§3.4), after terminal editors work (§3.3) |
| D2 | Port VS Code or Zed? | No (§3.2); Lapce and lite-xl as third-party source ports once the toolkit backends exist |
| D3 | Rust toolchain on Atrium | our own signed Rust layer for both arches (stable + the pinned nightly), rustup allowed on amd64; work toward upstream tier 2 for aarch64 |
| D4 | How tools get into a workspace | `pkg` as jail root inside the workspace behind a press, plus signed layers for C, Rust, Python, Go, Node (§6) |
| D5 | Cargo and other registries vs Nuntius | not through Nuntius's API; a new `registries` network mode with an allow-list proxy built on Nuntius's address policy; `outbound` stays the default |
| D6 | GPL tools (git, gmake, gdb, Emacs) | allowed in dev layers and workspaces, never in the base system or an app bundle; ship git and gmake in `c-dev` |
| D7 | The Atrium SDK | published crates (own sparse registry, offline copy in the layer), C headers, libraries and `.pc` files in the base, sysroot archives for cross builds, templates |
| D8 | Debugging | `develop` class with `ptrace` and per-jail unprivileged debugging for workspaces only (T1 now) |
| D9 | Debugging apps | Run under debugger for `dev.` apps only, never installed apps |
| D10 | Credentials | SSH keys in Arca through an agent socket per workspace and grant; tokens as app-held grants; SSH preferred for pushing |
| D11 | W^X in workspaces | keep `allow_wx=0`; don't honour `wxneeded` until a real toolchain needs it |
| D12 | `atrium-edit` vs Artifex | both: `atrium-edit` stays the plain text editor, Artifex is for projects |
| D13 | Default terminal editor in layers | Helix in `c-dev`, `vi` everywhere, others by `pkg` |
| D14 | Order | T1, T2, T3, T4, T5, then T6–T10 |
