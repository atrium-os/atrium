# Artifex — reference Insula IDE

Status: design sketch (pre-implementation).
Last updated: 2026-05-21.

Artifex (Latin: *skilled artisan / maker*) is the reference
Insula development environment — a native Pergola app that
serves as Insula's macOS-first bring-up flagship (see
`insula.md` §0.7.3) and as the demonstration that desktop-
class developer tools do not need Electron (see `insula.md`
§10.9).

Paired with **Opifex** (Atrium's package manager) via the
"-fex" *one-who-makes* suffix: Opifex makes and distributes
the artifact; Artifex is the workshop where the artifact is
made.

## 0. Scope and positioning

### 0.1 What Artifex is

- A native Pergola application that edits source code,
  embeds a terminal, integrates language services and
  debugging, and supports a typed-capability extension
  ecosystem.
- The reference *implementation* against which the Insula
  spec's claims about dev-tool performance and capability
  modeling are tested.
- The bring-up flagship for the Insula contract on macOS,
  Linux, and Windows host adapters (`insula.md` §0.7).

### 0.2 What Artifex is not

- **Not a text editor.** `atrium-edit` is the foundation
  text editor (per `NAMING.md`); it stays simple. Artifex
  is the IDE-shaped sibling. They share the Pergola text-
  rendering primitives and possibly the buffer model, but
  Artifex layers on language services, debugging, the
  extension API, project model, and the rest.
- **Not a platform.** Artifex *uses* Insula primitives
  (Limen, Aqueduct, Pergola, Stoa, Opifex, Vestibulum); it
  does not invent new ones. If a feature would require
  Artifex-specific platform mechanism, that mechanism
  belongs in Insula (or its dependencies), not in Artifex.
- **Not an Electron alternative in shape.** Artifex is not
  "VS Code but native." It is a Pergola app whose
  *capabilities* match or exceed VS Code's; its UI and
  workflow may differ where Pergola conventions differ
  from web conventions.

### 0.3 Status and bring-up posture

Pre-implementation. The existing `atrium-edit` codebase
(buffer model, keymap, glyph cache, `fresco-socket-rs`
client) is the foundation Artifex grows from. Bring-up
plan in §11.

## 1. Architecture — multi-process

Artifex is a normal Insula app from Portcullis's
perspective, but a *complex* one internally. Its
deployment is a tree of jailed processes coordinated via
Aqueduct.

```
Artifex (parent jail)
│
├── Editor process              — Pergola UI, document model, command bus
│
├── Limen-embedded children     — one jail each
│   ├── Stoa terminal session(s)
│   ├── Extension processes (one per extension)
│   └── Document preview / Markdown render (atrium-doc embed)
│
├── Sibling-jail children       — coordinated by editor, not embedded
│   ├── LSP server processes    — one per active language
│   ├── DAP adapter processes   — one per debug session
│   ├── Search worker           — ripgrep-shaped
│   └── Indexer                 — tree-sitter parses + symbol index
│
└── Resident state (Tessera namespace)
    ├── workspace.cas/          — opened files, content-addressed
    ├── settings/               — Curia-backed
    └── transient/              — recent files, layouts, breakpoints
```

**Trust topology:** the editor process is the trusted core
within Artifex. Limen-embedded children render into
allocated slots but cannot read editor state. Sibling-jail
children (LSP, DAP, search, indexer) are not embedded;
they communicate via typed Aqueduct messages and are
sandboxed against the workspace fd only (no broader
filesystem access).

A crash or hang in any child process must not affect the
editor.

## 2. Editor surface

### 2.1 Text rendering

Pergola's text primitives (built on `fresco-text` from the
existing Atrium tree — rustybuzz + swash, per-glyph CAS
textures). Pergola handles font fallback, RTL, ligatures,
emoji.

### 2.2 Buffer model

Rope-based buffer with content-addressed checkpointing.
Each saved version is a Tessera content hash; undo/redo is
navigation between versions; collaborative editing (if
ever) reuses the version graph.

Targets:
- Open 1 GB log file in <100 ms (mmap, lazy fold).
- Edit at any position in <1 ms.
- Undo/redo 100 ops in <5 ms.

### 2.3 Selection and cursors

Multi-cursor first-class. Each cursor is a (position,
selection-extent) pair. Standard editor operations
(insert, delete, move-by-word) apply to all cursors
simultaneously. Vim-mode and Sublime-style cursor commands
both fit this model.

### 2.4 Syntax / structure

Tree-sitter parses the buffer incrementally on edit.
Highlighting is a tree-sitter query over the syntax tree;
themes are queries-to-colors mappings. Code folding,
bracket matching, structural selection ("expand to
enclosing scope") all derive from the syntax tree.

### 2.5 Other text-editor table stakes

Line numbers, ruler, indent guides, soft-wrap toggle,
whitespace visualization, occurrence highlighting,
breadcrumbs, minimap, gutter for diagnostics + VCS marks.
None of this is novel; it is Pergola widgets composed.

## 3. Language services (LSP)

LSP is the cleanest external-process protocol in the
industry. Insula's posture (`insula.md` §10.9) is that
LSP-as-Aqueduct-over-stdio is exactly the right shape;
Microsoft accidentally invented the protocol Atrium would
have wanted.

### 3.1 LSP server lifecycle

- Artifex detects the active language from file
  extension + content hints.
- For a language with no running LSP server, Artifex asks
  Portcullis to launch the appropriate one in a sibling
  jail.
- The LSP server's manifest declares: workspace read
  access, network (if the server needs to fetch deps),
  CPU/RSS limits.
- Communication is JSON-RPC framed in Aqueduct messages
  (no transport translation — just envelope).

### 3.2 What LSP servers see

- A workspace fd granted by Artifex (Scrinium powerbox
  derived).
- A back-channel Aqueduct connection to the editor for
  diagnostics, completions, hover, code actions, etc.
- *Nothing else.* No network unless declared. No access
  to other jails. No raw filesystem.

### 3.3 Discovery and install

Language-server bundles are normal Opifex packages with
the `language-server` role declared. Installing a Rust
project triggers a prompt to install `rust-analyzer` if
not present; Artifex acquires it via Opifex.

### 3.4 Multi-root, multi-language

Multiple LSP servers run in parallel — one per active
language, scoped to the workspace. A workspace touching
Rust + TypeScript + Python has three LSP processes.
Inactive servers are reaped after idle.

## 4. Debugging (DAP)

Same shape as LSP. DAP adapters are jailed sibling
processes; the editor speaks DAP wire protocol to them.

### 4.1 Wrinkle — ptrace inside a jail

Debugging a user's Insula program requires ptrace-class
operations on the debuggee. Portcullis normally blocks
this. The debug session gets an explicit `debug`
capability that lets the DAP adapter ptrace within its
own jail subtree (the debuggee jail is a child of the
adapter's jail; the parent has debug authority).

The debuggee's jail capabilities are otherwise unchanged —
the program runs with its normal manifest, just observable.

### 4.2 Debug UI

Standard breakpoint gutter, variable inspector, call
stack, watch expressions, REPL via the DAP `evaluate`
request. Pergola widgets; no special platform support
required.

## 5. Terminal — Stoa via stoactl-gui

A Stoa persistent session presented inside Artifex via
the Limen embed mechanism. Stoa itself does not have a
direct "embed me" mode (it is designed for
attach/detach), so the mechanism is:

- Artifex requests `request_embed("terminal", rect, ...)`.
- Limen launches **`stoactl-gui`** (Stoa's existing
  graphical client; per `stoa.md` Phase S6) as a normal
  jailed Insula app in embed mode.
- `stoactl-gui` attaches to a Stoa session scoped to
  Artifex's workspace and renders the terminal surface
  into Limen's allocated slot via Pergola + Fresco
  ExternalSurface.
- Input routes via Limen's standard policy (§10.3.4 of
  `insula.md`).
- Session is persistent — survives both Artifex restart
  *and* `stoactl-gui` restart, since the session state
  lives in `stoad`.

Multiple terminal tabs = multiple Limen embeds → multiple
`stoactl-gui` instances → multiple Stoa sessions (or
multiple attachments to the same session if the user
prefers).

This design **does not require Stoa-protocol changes**;
it uses the existing stoactl-gui client and the
existing Limen pattern that any Insula app can be
embedded. Direct embed of a `stoad` session inside
another app's window is a possible Stoa protocol
extension but not required by Artifex.

The user gets full shell access in the workspace, with
all the Atrium tooling (`atrium-info`, etc.) reachable
because `stoactl-gui`'s manifest includes "workspace
fd + shell access" for the session.

## 6. VCS

Native git integration via libgit2-equivalent (Rust:
`gix`). The git binary remains available in the embedded
Stoa terminal for advanced operations.

### 6.1 Built-in views

- File-tree decorations (modified, added, untracked).
- Diff view (Pergola widget, custom-drawn region).
- Commit / amend / push UI.
- Blame layer over the buffer.
- History / log viewer.
- Conflict resolution editor.

### 6.2 Out of scope for v0

- Code review UIs (GitHub PR view, etc.) — these come
  from extensions (`github-pr` extension via Limen).
- Multi-repo workspace orchestration — extensions.

## 7. Extension model — Limen `editor-extension` role

The Limen role catalogue in `insula.md` §10.3.2 lists
`editor-extension` as one of the initial roles. This
section makes it concrete.

### 7.1 What an extension is

A normal Insula app with the `editor-extension` Limen role
declared in its manifest. Extensions run in their own
jails, may be written in any language, are sandboxed
against the workspace fd by default, and may declare
additional capabilities (network, additional fs paths,
shell access, etc.) which the user reviews at install.

### 7.2 What Artifex offers extensions

The `editor-extension` role exposes a typed Aqueduct
message channel between Artifex (parent) and the extension
(child). Initial protocol:

| Direction | Message | Effect |
|---|---|---|
| Artifex → ext | `init(workspace, capabilities)` | session start |
| Artifex → ext | `document_opened(uri, content_hash)` | file opened in editor |
| Artifex → ext | `document_changed(uri, edits)` | edits applied |
| Artifex → ext | `command_invoked(id, args)` | user invoked an extension command |
| Artifex → ext | `selection_changed(uri, range)` | cursor moved |
| ext → Artifex | `register_command(id, label, when)` | add to palette/menus |
| ext → Artifex | `register_provider(kind, language)` | hover/completion/lens/etc. |
| ext → Artifex | `add_panel(id, position, label)` | sidebar / bottom panel slot request |
| ext → Artifex | `update_panel(id, contents)` | extension renders into its slot |
| ext → Artifex | `show_message(severity, text)` | toast / notification |
| ext → Artifex | `request_input(prompt, kind)` | quick-input |
| ext → Artifex | `apply_workspace_edit(edits)` | mutate buffers |

This is intentionally a *subset* of VS Code's extension
API — it covers what 90% of extensions use; the long tail
(SCM provider, custom views, webviews) come later.

### 7.3 UI slot rendering

When an extension requests a panel slot, Artifex
allocates a Limen child surface for that extension to
render into directly. The extension's UI is *its own
Pergola surface*, composed by the compositor — Artifex
never mediates extension pixels.

This is strictly stronger than VS Code's "webview" model
(where extensions inject HTML+JS into the editor process):
- Extensions cannot break the editor's UI.
- Extensions cannot inspect the editor's UI state.
- Extensions can use any language for their UI (Rust +
  Pergola native, or a higher-level binding).
- Performance is bounded by the extension's own jail
  (rctl limits).

### 7.4 Capability declaration

Extensions declare in manifest:

```toml
[app]
name = "com.example.rainbow-brackets"
version = "1.0.0"

[role]
implements = ["editor-extension"]

[editor-extension]
activates-on = ["language:rust", "language:typescript"]
providers = ["text-decoration"]
commands = ["rainbow-brackets.toggle"]

[capabilities]
workspace-read = true        # implicit for editor-extension
pergola = true               # implicit — extension renders into Limen slot via Pergola/Fresco
network = false              # explicit
shell = false                # explicit
```

Network-requiring extensions (e.g., a Copilot-shaped AI
assist) declare hosts; the user sees them at install.

The `pergola = true` capability is implicit for any
`editor-extension`-role extension because the role
contract requires the extension to render its own
surface into Artifex's allocated Limen slot. Portcullis
must grant the extension jail access to the Fresco
socket; the Insula SDK abstracts this so extension
authors do not declare it manually.

### 7.5 Extension performance and isolation

Extensions run in jails with default `rctl` limits:
- 100 MB RSS
- 1 CPU-second per 10 wall-seconds (idle background)
- 30 s startup grace before limits kick in

These are tunable per-extension by user override.

A crashing or runaway extension cannot affect Artifex:
the editor process keeps running; the extension's panel
shows "extension stopped responding" with a relaunch
button.

### 7.6 Extension distribution

Extensions are normal Opifex packages, distributed
through any registry the user trusts. Artifex includes an
extension browser UI that surfaces extensions from
configured registries (default: a curated platform
registry).

## 8. AI assist

Two complementary paths:

### 8.1 First-party assist slot

Artifex ships with a built-in AI-assist surface (inline
completions, chat panel, edit-by-instruction). The
*backend* is configurable:

- Anthropic Claude (default suggestion)
- OpenAI
- Local model via `llama.cpp` / `mlx` / etc.
- Any provider speaking a documented assist protocol

User picks at first-run; switches in Curia settings.
Provider configuration includes API key (stored in
Vestibulum's keychain, never exposed to extensions or the
editor's transient memory).

### 8.2 Extension-based assist

The `editor-extension` role lets any extension implement
assist features through the `register_provider("inline-
completion", ...)` and related hooks. Multiple assist
extensions can coexist; user picks active one in
settings.

### 8.3 Privacy posture

Assist features that send code to a remote provider are
**loudly disclosed** in the manifest and at install. The
status bar shows when a remote assist is active. No
silent telemetry of code contents.

## 9. Workspace model

A "workspace" is a directory tree the user has explicitly
handed Artifex, via Scrinium. Artifex cannot see the
user's whole filesystem.

### 9.1 Workspace contents

```
my-project/                     ← user's project directory
├── src/                        ← source files
├── tests/
├── Cargo.toml
└── .artifex/                   ← Artifex-managed
    ├── workspace.toml          ← workspace settings
    ├── extensions.toml         ← per-workspace extension config
    ├── breakpoints.json
    └── tessera/                ← Tessera namespace mounted here for transient state
```

`.artifex/` is opt-in (Artifex creates it on first save
of workspace state; users can opt out and store state
elsewhere).

### 9.2 Multi-workspace

Multiple workspaces open simultaneously, each in its own
top-level editor tab/window. LSP servers are per-language-
per-workspace.

### 9.3 Remote workspaces

A workspace can live on a remote Atrium host (per
`insula.md` §20.2). Artifex's editor connects to a
remote-rendered editor session, *or* a local editor
connects to remote LSP/DAP/Stoa over Aqueduct (more
useful — local UI, remote compute). The latter is the
"VS Code Remote" pattern, native.

## 10. Performance targets

Restating `insula.md` §10.9.5 with Artifex-specific
expansions:

| Metric | VS Code (Electron) | Artifex target | Validation |
|---|---|---|---|
| Cold start | 2–5 s | <100 ms | startup benchmark |
| Idle RAM | 200–500 MB | 20–50 MB | RSS at idle |
| Open 100 MB log | chokes | instant (mmap) | open + scroll-to-end |
| Open 1 GB log | impossible | <100 ms first paint | same |
| Edit latency | 30–80 ms (perceived) | <5 ms | keypress-to-paint |
| Idle battery | non-trivial | near-zero | wattmeter |
| Extension spin-up | 100s of ms | ~5 ms (jail pool) | Limen launch benchmark |
| LSP completion roundtrip | 50–200 ms | 10–50 ms | LSP benchmark |
| Search across 100k files | 5–20 s | <500 ms (ripgrep) | benchmark |

These are not aspirational; they are what native code on
modern hardware delivers when no JavaScript runtime sits
between the user and the work.

## 11. Bring-up phases

### 11.1 Phase A — minimum viable Artifex (target: macOS-first MVP)

- Open / edit / save files in a workspace granted via
  Scrinium.
- Pergola text rendering at editor quality (incl. ligatures,
  emoji, RTL via fresco-text).
- Rope buffer; multi-cursor; basic find/replace.
- Tree-sitter syntax highlighting for ~5 languages
  (Rust, TypeScript, Python, C, Markdown).
- Stoa terminal embed (one tab).
- One LSP server integrated end-to-end (`rust-analyzer`).
- Native git status decorations.
- Native performance (meets §10 targets).
- No extension API yet.

**Goal:** developers can write Insula code in Artifex,
with rust-analyzer support, on macOS. Showable demo.

### 11.2 Phase B — working IDE

- Multiple LSP servers in parallel.
- DAP debugger integration with `rust-gdb`/`lldb`-shaped
  adapter.
- Code intelligence: go-to-definition, find-references,
  rename, completions.
- Search across workspace (ripgrep worker).
- Multiple terminal tabs.
- Git: diff view, commit, push.
- Multi-cursor editor commands.

**Goal:** Artifex is a serviceable IDE for working Insula
developers. Equivalent to VS Code minus the extension
ecosystem.

### 11.3 Phase C — extension API

- Limen `editor-extension` role frozen at v1.
- Extension SDK + sample extensions (Rust, C).
- Extension manifest and capability declaration.
- UI panel slot allocation.
- Extension browser / installer.
- First few first-party extensions: GitHub integration,
  Markdown preview (using atrium-doc), AI assist (slot
  for provider).

**Goal:** ecosystem can begin to grow around Artifex.

### 11.4 Phase D — ecosystem maturity

- AI assist productionized.
- Refactoring framework (multi-file edits, structural
  search-and-replace).
- Profile / inspect integration (dtrace-backed perf
  view).
- Multi-window, multi-workspace.
- Workspace-trust UX (untrusted workspace = no auto-LSP,
  no auto-task-run).
- Linux + Windows host adapter support.

**Goal:** Artifex is competitive with VS Code on every
axis a developer cares about.

### 11.5 Phase E — Atrium integration

- Atrium GPU ABI accelerated rendering paths.
- Tessera-aware workspace operations (CAS-checkpointed
  history, content-addressed pin/share of code
  artifacts).
- Portcullis-native sandbox (vs the macOS / Linux host
  adapter).
- Stoa-deep integration: opening a Stoa session from any
  device returns the same workspace state.

**Goal:** Artifex on Atrium is *demonstrably* better than
Artifex on other hosts — showcasing what Atrium uniquely
offers.

## 12. Open questions

- **Buffer model details.** Rope is the leading choice; piece
  table is a contender. Both work; the tiebreaker is mmap
  integration for huge files.
- **LSP message transport.** JSON-RPC envelope over
  Aqueduct (transparent) vs. translation to typed Aqueduct
  messages (more typed-checked, more work). Probably
  envelope-passthrough for v0, typed for v2.
- **Extension API surface freeze policy.** When does v1
  ship and stop changing? Likely after Phase C lands and
  ~10 real extensions exist.
- **Curia settings vs. per-workspace files.** VS Code has
  global + workspace + folder + file settings. Artifex
  probably has Curia (global) + workspace (`.artifex/`)
  + folder. Detail TBD.
- **Theme model.** Tree-sitter-query-to-color mappings;
  loadable as data files; user-installable. Format TBD.
- **Keymap customization.** Modal (vim-flavored) and
  flat (VS Code-flavored) both supported; declarative
  key-to-command mapping; user-loadable. Format TBD.
- **Multi-language extension SDK.** Rust SDK first; C SDK
  follows; Python/Lua bindings via interpreter-extension
  pattern when those interpreters ship as Insula apps.
- **Plugin marketplace registry shape.** Opifex
  sub-namespace? Separate registry? Curated by whom?

## 13. References

- `docs/spec/insula.md` — parent spec; §10.9 is the
  Artifex pitch summary, §0.7 the bring-up strategy.
- `docs/spec/pergola.md` — UI toolkit; Artifex's surface.
- `docs/spec/aqueduct.md` — IPC substrate; how Artifex
  talks to its children.
- `docs/spec/portcullis.md` — jail launcher; how Artifex
  is itself sandboxed and how it spawns sub-jails.
- `docs/spec/stoa.md` — persistent sessions; terminal
  backend.
- `docs/spec/atrium-pkg.md`, `docs/spec/atrium-pkg-registry.md`
  — Opifex; how Artifex and its extensions are installed.
- Future sibling specs (planned):
  - `docs/spec/limen.md` — embed broker including the
    `editor-extension` role wire format.
  - `docs/spec/insula-host-macos.md` — macOS host adapter
    that Artifex's bring-up depends on.
