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
   with no network). Neither class has `fork`, `vfork` or `wait4` (only `terminal` adds them), so a shell can't start a command. Shells and compilers work today
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

### 3.5 Artifex's UI and assets (Shankar, 2026-11-24)

Accepted from the [design canvas](https://claude.ai/artifact/P1DtV5MRskZPA22pp1UcBc)
(editor window, composition, fuzz and Move, assets, toolbar states and icon
map). Built from Atrium's tokens, IBM Plex and the Phosphor font the base
already ships, so every icon exists.

- **Toolbar** (Pergola `Toolbar`): workspace and project pickers; Run as the
  one accent-filled button (it becomes Stop while the app runs, and holds the
  Run question); Build, Test, Debug as icon-and-label buttons; "Go to file or
  command" (Ctrl K); Terminal. Narrow windows drop labels first, then move
  Debug and Terminal into More; Run, the workspace and search stay.
- **View strip** down the left: Files, Search, Composition, Fuzz (with a
  crash count), Debug, Assets. Files get type icons (`code` for source,
  `file-text` for config and docs, folders in the accent mark colour); tabs
  show an unsaved dot; a path line sits above the editor; a status bar shows
  the workspace, `cargo atrium check`, the language server and the caret.
- **Composition** is a diagram: grants, the core (the only part with them),
  workers, each with its fuzz state. **Fuzz** shows the crash, its input as
  hex, and the Move into a worker preview (name, input kind, files changed,
  calls to change by hand, Move and Cancel).
- **Assets** (`icons/`, `images/` in the project): pictures with their scales,
  icons previewed at 16 to 48 px in light and dark, and where each is used.
  - **Import SVG** checks a file with the dock's own checker (navigator-svg's
    strict profile, forum.md §6.1). It converts what it can (text to outlines,
    strokes to fills, hidden layers dropped) and names what it can't keep
    (masks, patterns, scripts, outside references), offering to import the
    rest. What imports in Artifex shows in the dock.
  - **The icon editor** is an icon editor, not a vector app: lines, shapes and
    a pen on a 24-unit grid, fills and strokes, linear and radial gradients,
    Atrium's palette plus up to 8 colours of the project's own, a flat list of
    shapes to reorder. No masks, boolean operations, text, filters or effects.
    A counter shows the dock's 64-shape limit, and the editor refuses to save
    an icon the dock would refuse. Anything bigger is drawn in another tool
    and imported.
- **Pictures in apps**: the SDK's `assets::image("name")` asks the system
  image service (app-composition.md C6: navigator-image in its own jail) to
  decode a picture from the app's bundle and gets pixels back. The core never
  parses image bytes, and a developer needs no worker of their own for their
  own pictures.

Order: (1) toolbar, view strip, file icons; (2) Fuzz and Composition views;
(3) the image service and `assets::image`; (4) Assets with Import SVG; (5) the
icon editor.

As built (steps 1 and 2, `artifex/app/src/ui.rs`, `views.rs`): Files,
Composition and Fuzz in the strip switch the view right of it (`State.view`);
Problems and Debug open the panel under the code, which is now Output,
Problems and Debug only. A full view hides the panel; opening a file, a
problem, a frame, a task or Debug goes back to the code. The Fuzz view lists
targets and crashes, shows the chosen crash with its first 128 input bytes as
hex (read by the engine from the project), and puts `worker add --dry-run`'s
answer in a right-hand pane with Move and Cancel. The Composition view draws
the grants, the core (flagged with a core crash and its Move button), the
workers with their fuzz state, and the check's findings. The trace prints
`view=`, and `vm-artifex-check.sh` drives the Fuzz view's buttons and opens
the Composition view.

As built (step 3): the image decoder is `org.atrium.image-decode`
(`atrium-image-decode/`, installed by `scripts/image-decode-bundle.sh`),
started per session by portcullisd's `SystemWorker` for an app naming
`decoders = ["image"]` (app-composition.md §3, C6). It speaks the
`atrium-worker` wire: `atrium_assets::Decode { max_width, max_height }` plus
the open file, answered by `Picture { width, height, rgba }`. A raster
picture is fitted into the box and never enlarged; an SVG is drawn at the
largest size of its own shape that fits. The SDK's `atrium-assets` crate
opens `assets/<name>` in the bundle (names of `A-Z a-z 0-9 . _ -` parts,
no `..`), checks the answer's size and length, and with
`ATRIUM_WORKERS=local` runs the decoder as a plain child for development
off Atrium. `scripts/vm-image-decode-check.sh` is its VM check.
A system worker's own installed manifest must declare nothing (no
capability, not even one set to false, no `[[workers]]`, `[roles]` or
`[ipc]`), each app holds at most 4 at once under the rule's system-wide
cap, and a jail lives at most 5 minutes whatever its caller does
(`portcullisd::system_worker`).

As built (step 4, first half): `sdk/atrium-icon` is the dock's check
(`check`: shapes, points and bytes against `portcullis_toml::icon`'s
MAX_ICON_SHAPES 64, MAX_ICON_POINTS 8192 and MAX_ICON_BYTES 32 KB, which
forum-dock also uses) and the importer (`import`): it drops other
namespaces, editor metadata, hidden layers, scripts, handlers and outside
links, names each element it can't keep (text, filters, masks, images,
markers), and writes the rest as plain filled paths (transforms folded,
gradients kept in `<defs>`, rectangular clips, opacity, even-odd).
`cargo atrium icon check <svg> [--json]` and `cargo atrium icon import
<svg> [--name n] [--dry-run] [--json]` (writes `assets/icons/<n>.svg`)
are its CLI, and `cargo atrium bundle` now carries every file under
`assets/` (plain names, no links, at most 4096).

As built (step 4, the view): the strip's Assets (an `atrium.toml`
project) lists `assets/` (icons in `assets/icons/`, then pictures, then
other files; at most 500, links not followed). An icon shows at 16, 24,
32 and 48 points on a light and a dark ground, a picture fitted into
480×320; both are decoded at 2× by the system's decoder (Artifex's
manifest names `decoders = ["image"]`; one `atrium_assets::Decoder` on a
thread of its own), never in Artifex. Under each: where the project's
sources name it (a bounded search), and for an icon the dock's verdict
from `cargo atrium icon check --json` in the workspace. Import SVG lists
the project's SVGs outside `assets/`; picking one runs `cargo atrium icon
import --dry-run --json` and shows the result decoded, its counts against
the limits, what was converted and what wasn't kept; Import writes it.
`artifex::assets` is the model, `views::assets` the view;
`vm-artifex-check.sh`'s "Assets" section is its VM check.

As built (step 5, the icon editor; `artifex/app/src/iconedit.rs` the model,
`views::icon_editor` the view): New icon (`assets/icons/app.svg`, or the
first free `icon[-N].svg`) and Edit on an icon drawn here open the editor
in the Assets view's right pane. Tools: Select (press a shape, drag to move
it), Rectangle, Ellipse, Line (drag) and Pen (click points, click the first
to close; Done leaves it open as a stroked line); points snap to half units
of the 24-unit grid and stay on it. Fill is none, a colour, or a linear
(top to bottom of the shape) or radial (from its centre) gradient between
two colours; stroke is none or 1, 1.5, 2 or 3 units. Colours are the 12 of
Atrium's palette plus at most 8 of the icon's own (Lighter and Darker make
them). The shape list is flat, front first, with Front, Back and Delete.
The counter shows the dock's shapes against 64 (a stroke is a shape of
its own to the dock), and the icon is drawn
beside the canvas at 16, 24, 32 and 48 pt on light and dark through the
same Fresco fills the dock uses. Save writes plain filled paths (a stroke
is written as the fill it paints; gradients in `<defs>` in user space) and
refuses, saying why, an empty icon or one past the dock's shapes, points
or 32 KB; `atrium_icon::check` passes what it writes, counting the same
shapes and points (a unit test). The editor's own description of its
shapes is a JSON comment in the file (`artifex-icon v1`), read back with
bounds on every number; an icon without one was drawn elsewhere and is not
opened for editing (it is edited there and imported again). Save goes
through a temporary and a rename, and refuses a linked `assets/` or
`assets/icons/`. An unsaved icon keeps the pane until saved or discarded.
`vm-artifex-check.sh` draws one of each shape, reorders, saves, and checks
the saved icon is decoded and passes the dock's check.

### 3.6 Debugging in Artifex after the redesign, and inspecting an app's UI (Shankar, 2026-11-25)

Breakpoints (gutter, F9), F5 Debug and Continue, F10/F11/Shift+F11 and a
Debug panel of frames and top-level locals came before the redesign (M12,
§7). Accepted, in this order: the stepping VM check and the TCG fixes, then
A, then B.

**A. The debugger's UI.**
- D19: debugging stays a panel under the code (stepping is through code);
  the view strip gets a Debug entry that opens it, with a dot while the
  program is stopped.
- D20: while a program is debugged the toolbar's controls are one group
  with icons and key hints: Continue or Pause, Over, Into, Out, Restart,
  Stop.
- D21: the editor marks the stopped line (an arrow and a highlight),
  breakpoints are dots in the gutter, and hovering a name while stopped
  shows its value (DAP `evaluate` in the hover context).
- D22: the panel is three columns: the call stack; variables that expand
  (structs, arrays, Rust's `Vec` and `String`, through DAP
  `variablesReference`); breakpoints, each with a tick to disable it, a
  condition, and Remove. Under them, a line evaluates an expression in the
  selected frame.

**B. Inspecting a dev app's interface.**
- D23: Inspect, on the toolbar while a dev app runs: pick an element in the
  app's window and see what it is, its box, colours and fonts, and the
  line of the app's source that drew it (a jump to it); a live tree of the
  app's interface, a row hovered outlines its element in the app; a toggle
  that draws every element's bounds.
- D24: read-only to start; no live editing of styles.
- D25: only a dev build, only for the Artifex that started it. Pergola
  records source lines only in dev builds, so a shipped app carries none;
  Artifex reaches the app inside the workspace as the debugger does, so
  nothing new is opened to other apps.

**As built (2026-11-25).**
- *Debug under emulation.* Two causes kept the VM check's Debug red on the
  cloud's TCG guest. lldb gives up on the gdb-remote handshake after about
  6 s, and the first reply comes only once portcullisd has made the app's
  jail and lldb-server has started the app; a timed-out handshake drops the
  connection, and lldb-dap then waits out its attach `timeout` for a stop
  that cannot come. Artifex's attach now sets
  `plugin.process.gdb-remote.packet-timeout` to 60 s first
  (`dap::HANDSHAKE_S`) and gives the attach 120 s. And a base older than
  2a7b458a7 has the `atrium.app.mk` whose `CFLAGS?= -O2 -g` loses to
  sys.mk's, so the app had no line table; the check now installs this
  tree's rules, in the base and in each workspace's own `/usr/share/mk`.
  Debug builds a make project with `make ATRIUM_DEBUG=1`, which
  `atrium.app.mk` turns into `-O0` (its objects depend on the flags they
  were built with, so the next plain build redoes them): at `-O2` lldb
  showed most locals as optimised away. The check steps from the
  toolbar (Into `span_of`, Out to main, Over to a later line, Continue to
  the next frame, Stop ends lldb-dap and the app's jail). Toolbar, not
  F-keys: with frescod alone the app's window keeps the keyboard.
- A Makefile with a `run:` target of its own gets a Run task, as one with
  `atrium.app.mk` does.
- *A.* The toolbar group (`ui.rs`) replaces Debug while a session is live;
  New project and Terminal go to icons then. The editor (`CodeGrid`) draws
  the stopped arrow, a ring for a breakpoint that is off, and a note box
  under the hovered name (`on_point`, through Pergola's new
  `Ctx::on_pointer_local`); Artifex evaluates the name under the pointer
  (`a.b`, `p->q` included) in the shown frame. The panel
  (`views::debug_panel`) has the stack, variables that open (members fetched
  per `variablesReference`, at most 200 a level, 64 levels open; what was
  open stays open across steps), and breakpoints with a tick, a condition
  field and Remove; the evaluate line shows the last result. lldb's
  `watch` context gives bare values (`repl` and `hover` add `(int) $0 =`).
- *B, the transport.* `WorkspaceInspect` is Run with a socket: portcullisd
  makes a socketpair, puts one end at the dev app's fd 3 through jaild's
  extra descriptors (`ATRIUM_INSPECT_FD=3`) and sends the other to the
  app that pressed Run, after `WorkspaceRan`. Only `dev.` apps. It is a
  descriptor, never a path, so only that Artifex holds it; Artifex's Run
  always asks for it, and an app that doesn't answer (a C app) is just Run.
- *B, the app's side* (`pergola::inspect`, compiled only with debug
  assertions). `Ctx::add`/`push` are `track_caller` in a dev build and
  record each node's line; every Pergola widget's `render` is
  `track_caller` and calls `Ctx::widget_site` first, so what a widget draws
  is put down to the app's line that drew the widget, never Pergola's own;
  a node a widget inserts straight into the tree takes its parent's line.
  The window reads the socket between events (every 200 ms while it is
  open): `Tree` (every node's kind, window box, colours, fonts and text in a
  line, and line), `Pick` (the next press in the window is the
  inspector's, answered with the deepest node under it), `Highlight` and
  `Bounds` (outlines drawn over everything as extra nodes, which the tree
  and picks leave out). Frames are a `u32` length and postcard, at most
  8 MiB, 20 000 nodes; a bad frame drops the IDE.
- *B, Artifex.* An Inspect entry at the end of the view strip, once the app
  has answered; the view (`views::inspect`): the tree (rows fold, the wheel
  scrolls, a hovered row outlines its node in the app), Pick, Bounds,
  Refresh, and the chosen node's box, look (with swatches) and line, with
  Go to source for a file of `work`. When the app ends its last tree stays,
  marked so. The app is bounded as a peer (`inspector.rs`).
- Deviations from D23/D25 as written: Inspect is a full view from the strip,
  like Fuzz and Assets, rather than a toolbar button; and Artifex reaches
  the app by the socket portcullisd hands it rather than through the
  workspace, which keeps D25's point (nothing new is open to other apps)
  without putting Artifex in the workspace.
- *C apps.* libatrium answers too (`libatrium/src/inspect.rs`): each
  `atrium_window_frame_*` call (and `atrium_window_fill_rect`) is a node,
  flat, in drawing order, with its box (a path's turned quad boxed, a
  glyph run's glyphs), colour and the app's line. `atrium.h` wraps those
  calls in macros that first pass `__FILE__` and `__LINE__` to
  `atrium__site` (`ATRIUM_NO_SITES` turns them off); libatrium reads the
  name only while an IDE listens, and records nothing otherwise. A press
  while Pick waits picks the last-drawn node under it; Highlight and
  Bounds are drawn as thin rects with ids from `0xFFFF0000` just before
  the frame ends, and are never listed. C apps draw in device pixels;
  the IDE is told points, as from Pergola (libatrium divides by the
  window's scale).
- The wire (`Ask`, `Tell`, the framing and the app's end of the socket)
  is its own crate, `atrium-inspect`, which Pergola, libatrium and
  Artifex share.
- `vm-artifex-check.sh` covers both: stepping as above, then Run on the
  app template's Pergola app: portcullisd puts the socket at fd 3, the app
  answers with its tree, the strip's Inspect shows it, Pick and a press on
  the app's button give its Text at the button's line in `ui/src/main.rs`, Bounds, Go to
  source opens that line, and the ended app's tree stays marked ended. Then
  Run on the c-app template's app: its frame answers, and Pick and a
  press on its background give the `Rect` drawn at its
  `atrium_window_frame_rect(1, ...)` line (the C SDK in the base must be
  this tree's: `install-c-sdk.sh`).

### 3.7 Language server features in Artifex (Shankar, 2026-11-25)

Shankar picked these after the debugger and inspector: completion first,
then signature help, references, rename, quick fixes and format on save,
checked with rust-analyzer and clangd. As built:

- **Protocol** (`artifex/app/src/lsp.rs`): the client asks for completion
  (snippets off; any snippet that comes anyway is made plain text by
  `strip_snippet`, nesting at most 8 deep), `signatureHelp`, `references`,
  `rename`, `codeAction` (with the diagnostics in the range) and
  `formatting`. A workspace edit is kept only for files in `work`; create,
  rename and delete operations and command-only code actions are left out
  (Artifex runs no server commands). A newer completion request cancels
  the older one (`$/cancelRequest`), and an answer to anything but the
  newest completion or signature request is dropped. Answers are bounded:
  1000 completions, 2000 locations or edits, 200 actions, 1 MiB per
  edit's text, 64 Ki chars per completion's text.
- **Editor widget** (`pergola::widgets::code_grid`): a `Menu` under the
  word (above it near the bottom), 8 rows, the selected one kept in view,
  details right-aligned; a `Tip` above the line with the active parameter
  bold; `intercept_keys` sees keys before the buffer, and a press on a row
  calls `on_menu_pick`. `CodeBuffer::replace` applies a list of edits as
  one undo step (overlaps dropped, ranges clipped).
- **Artifex** (`assist.rs`, `engine.rs`): typing asks for completions at a
  word's second letter (once per word, again while the server says its
  list is incomplete) and after `.`, `::` or `->` when the server lists
  those triggers; Ctrl+Space asks anywhere. The menu narrows as you type
  (prefix, then case-insensitive prefix, then letters in order, then the
  server's sort) and closes when the caret leaves the word. Up, Down,
  Page Up/Down move in it, Enter or Tab accept, Escape closes it. The
  server's trigger characters for signatures (`(`, `,`) show the tip; `)`,
  leaving the line or Escape hide it. Shift+F12 lists references in a
  References panel (a click goes there; a rename, fix or formatting moves
  the listed places with the text); F2 opens a rename bar over the
  editor (Enter or Rename; only a plain name is sent); Ctrl+. offers quick
  fixes for the caret's line in the same menu, the server's preferred one
  first (clangd marks a missing `;` on the next token, so its fix is
  there). Shift+Alt+F
  formats; Ctrl+S formats first when the file's folder or one above it in
  `work` has `rustfmt.toml` / `.rustfmt.toml` (Rust) or `.clang-format` /
  `_clang-format` (C, C++),
  and saves as it is if the server hasn't answered in 3 s or the text
  changed meanwhile. Edits to files not open open them; every changed file
  is told to the server, and the note says how many changes in how many
  files. The status line shows the active file's server and what it is
  doing (starting, ready, ended), or none.
- **Checks**: `engine::tests::language_server_features_with_a_scripted_server`
  drives every feature against `artifex/app/tests/fixtures/fake_lsp.py`
  (started as the Python server), and
  `language_server_features_with_clangd_when_there_is_one` does rename, a
  quick fix, completion, signature help and format on save with a real
  clangd; `vm-artifex-check.sh` has a section that
  does references, rename, a quick fix, completion, signature help and
  format on save with clangd on `names.c`, and completion after `Vec::`
  with rust-analyzer.

Not built: plugins (no design yet), workspace symbol search, inlay hints,
semantic tokens, server commands.

### 3.8 Plugins for Artifex (proposal, 2026-11-25)

Status: ACCEPTED by Shankar 2026-11-25 (D26–D30 as recommended); being built. It makes artifex.md §7
(the `editor-extension` Limen role) concrete now that Limen, the LSP
features (§3.7) and Pergola slots are built, and changes it in two places
(D27, D28).

**What a plugin is.** An app bundle whose manifest implements the
`editor-extension` role. It runs in its own jail, started by limend when
Artifex opens a file of a language it names, and stopped when Artifex
closes or the plugin is switched off. It is written in any language; the
SDK gives Rust a crate (`atrium-artifex-ext`) and C a header.

```toml
[app]
id = "org.example.todo-lens"
entry = "bin/todo-lens"

[roles]
implements = ["editor-extension"]

[roles.editor-extension]
languages = ["rust", "c"]          # what it is started for; "*" for all
commands = ["todo-lens.list"]      # shown in the command palette
panel = "TODOs"                    # it may draw a panel (a Limen slot)
workspace-read = false             # D28: off unless asked for and granted
# network: the app's own [capabilities] network, shown when it is switched on
```

**What it speaks (D27).** Language Server Protocol, over the role's
channel instead of a workspace process's stdio, plus three Artifex
messages: `register_command`, `command_invoked` and `panel` (open, close,
size). Artifex already turns LSP into completion, hover, diagnostics,
references, rename, quick fixes and formatting, so a plugin gets all of
that UI by answering the same requests a language server does, and an
existing language server written for stdio becomes a plugin with a thin
wrapper. Files are named `artifex:///<path in work>`; the text comes in
`didOpen`/`didChange`, never by reading the disk. Edits a plugin makes
(workspace edits, code actions, formatting) are applied by Artifex as
for a server: one undo step per file, only files in `work`, the note
says how many. A plugin never runs a server command in Artifex and never
runs a command in the workspace in v1.

**What it can reach (D28).** By default only what Artifex sends it: the
open files of its languages. `workspace-read = true` asks for a read-only
view of the workspace's `work` (a nullfs mount in its jail), granted per
workspace when the person switches the plugin on there, and shown in the
Plugins view. No write access, no home, no secrets. Network only to the
hosts its manifest names, shown before it is switched on (D30). Limits:
256 MiB, a CPU share, 64 KiB messages and 200 a second each way; a
plugin over them is stopped and its panel says so, with Restart.

**Its UI.** A panel is the plugin's own Pergola window in a Limen `plain`
slot in Artifex's bottom panel (a tab with its name) or as a full view
from the strip: Artifex never draws for it and it never sees Artifex's
window. Commands go into a new command palette (Ctrl+Shift+P), which
lists Artifex's own acts too. Diagnostics, completions and the rest show
where the LSP ones do, marked with the plugin's name.

**Many at once (D29).** Unlike other user roles (one default provider),
every plugin the person switched on runs for its languages, side by side
with the workspace's language server. Artifex merges their answers:
completions in one menu (each row marked with its source), diagnostics
in one list, quick fixes in one menu; for formatting and rename the
workspace's server wins, then the plugin switched on first.

**Getting and managing them.** Plugins are Opifex packages like any app
(signed, from a registry the person trusts). A Plugins view in Artifex
lists the installed ones with what each asks for, switches them on or
off per workspace, and shows a stopped one's reason.

**Why not in Artifex's process (D26).** WebAssembly plugins in Artifex
(Zed's way, with wasmtime, Apache-2.0) would start faster and need no
Limen hop, but they would run inside Artifex's jail, which holds every
picked workspace, and a runtime bug would be a way out. A jail per plugin
is how every other third-party code runs on Atrium; the cost is about
one process and one channel per plugin.

**Build steps if accepted.**

- P1: the role's message types in `limen::editor_ext` (LSP frames as
  bytes, the three Artifex messages, bounds), limend's catalogue with
  a provider rule for "every plugin switched on" (D29), the manifest
  section.
- P2: Artifex's plugin host: start plugins for a file's language through
  Limen, the LSP client over the channel, answers merged with the
  server's.
- P3: the command palette and plugin commands.
- P4: plugin panels in Limen slots.
- P5: the Plugins view, `workspace-read` grants, limits and restart.
- P6: the SDK crate, two sample plugins (TODO lens with a panel; a
  Markdown link checker as diagnostics), a VM check.

**As built.** P1 (2026-11-25): `limen::editor_ext` (messages, LSP
parts of ≤60 KiB joined up to 4 MiB, bounds), limend's
`Provider::Plugin` (`REQUEST_PLUGIN` names the plugin, checked against
installed manifests for the parent's person, one of each per window;
limen.md §2.7), `[roles.editor-extension]` in portcullis-toml, and
`embeds = ["editor-extension"]` in Artifex's manifest.
P2 (2026-11-25): `artifex::plugins::Host` starts the switched-on
plugins for an open file's language through a `Link` (limend's
`LimenLink` in the app), gives each its own LSP client with
`artifex:///` URIs, and stops one that breaks the framing (the reason is
kept, and it isn't started again until Restart). The engine merges
completions (rows marked with the plugin's name, ranges recoded to the
file's first client's encoding), quick fixes (each applied in its own
encoding) and diagnostics; definition, hover, references, rename,
signature help and formatting go to the workspace's server, else the
first plugin. Tested with a scripted plugin beside the scripted server.
P3 (2026-11-25): the command palette (Ctrl+Shift+P, `artifex::palette`)
lists Artifex's own acts with their keys and each running plugin's
registered commands (only those its manifest lists) with its name; typed
letters match in order, a word's start first, 12 rows at most. A plugin
command goes as `Command { id, file, line }` for the active file.

**Decisions.**

- D26: plugins run in their own jails through Limen, not in Artifex's
  process. Recommended.
- D27: the v1 protocol is LSP plus commands and a panel, replacing
  artifex.md §7.2's own message table. Recommended: it reuses the UI just
  built and existing servers.
- D28: no workspace files by default; `workspace-read` asked for in the
  manifest and granted per workspace; no writes or commands in v1.
  Recommended (artifex.md §7.4 had workspace read implicit).
- D29: all plugins switched on run together, answers merged.
  Recommended.
- D30: network only to the hosts the manifest names, shown when it's
  switched on. Recommended.

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
  manifest lists them by file name, `*` and `?` allowed
  (`[bundle] extra-libs = ["libjansson.so.4", "libgdk_pixbuf-*.so*"]`). Each is looked
  up where a linked library would be (`lib`, `usr/lib`, `usr/local/lib`, and the
  entry's run paths), goes in with what it needs in turn, and a pattern that matches
  nothing is an error. Name the soname rather than `.so.*`, which also matches the
  versioned file and carries it twice.
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
| D15 | Artifex's UI (2026-11-24) | ACCEPTED: toolbar, view strip, diagram composition, assets view (§3.5) |
| D16 | Icons drawn in Artifex | ACCEPTED: filled, multi-colour icons, capped at what the dock draws (64 shapes; no masks, booleans, text, filters) |
| D17 | Pictures from an app's bundle | ACCEPTED: `assets::image` through the system image service (C6), never decoded in the core |
| D18 | Importing SVG from other tools | ACCEPTED: the dock's checker; convert what it can, name what it can't keep |
| D19–D22 | Artifex's debugger UI (2026-11-25) | BUILT: a panel with a strip entry, one toolbar group, the stopped line and hover values, stack, expanding variables, breakpoints with conditions, an evaluate line (§3.6) |
| D23–D25 | Inspecting a dev app's UI (2026-11-25) | BUILT: pick, tree and bounds; read-only; dev builds and their own Artifex only (§3.6) |
| D26–D30 | Plugins for Artifex (2026-11-25) | ACCEPTED: jailed apps through Limen speaking LSP plus commands and a panel; no workspace files unless granted; many at once (§3.8) |
