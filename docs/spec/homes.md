# Homes, workspaces and consoles

Status: accepted 2026-11-15 (Shankar: "accept as recommended", H4–H7 as recommended), from a discussion with Shankar ("you work in an
app context, not a user context"; "a standard terminal defeats the purpose of
jailed apps"; per-app data under the person's home: yes). W1 built
2026-11-15 (§8): overlays at `~/.atrium/apps/<id>`, the app uid's, 0700,
moved from the machine-wide place on first launch; session state at
`~/.atrium/state`; the five folders and `files-<person>` with `home-files`;
gate `scripts/vm-homes-check.sh` 0 failed. On FreeBSD a folder's group
always passes to what is made in it, so 2770's setgid bit only documents
the intent there.

Related: [portcullis.md](portcullis.md) §4 (app jails and overlays), §9.0
(per-app uids); [forum.md](forum.md) §6 (session components' state folders);
[atrium-devjail.md](atrium-devjail.md) (dev-jails, which become workspaces
here); [scrinium.md](scrinium.md) §9 (home folders, the files group,
bookmarks).

## 1. The idea

On a usual system you log in and work as yourself: your shell, your
compiler and every program you start run as your uid and can read all of
your home. In Atrium nothing runs as the person. A person is an identity
that owns data and policy (portcullis.md §9.0); what runs is always an app,
in its own jail, as its own uid, with the capabilities its manifest was
granted. So work happens in an **app context**, never in a user context, and
there is no general terminal: a shell that could reach everything the person
owns would undo every jail.

Three kinds of terminal remain, each inside a boundary:

| | what | runs as | sees |
|---|---|---|---|
| **App console** (§4) | right-click an app's icon, Terminal | that app's uid, in that app's jail | the app's own files and capabilities, nothing more |
| **Workspace** (§5) | a named jail with a toolchain and a folder of files, for "write a C program and run cc" | the workspace's uid | its own folder, the workspaces it links (§5.2), its declared network |
| **Host shell** (§6) | administration and recovery | root | everything; behind Sigillum, never part of daily use |

## 2. The home

`/home/<person>` stays: it is the person's `pw_dir`, as POSIX expects, and
FreeBSD's tools find it there. What changes is what it holds. Since nothing
runs as the person, the home is **storage only**:

```
/home/<person>/                  person:person 0700
  Documents/ Downloads/ Pictures/ Music/ Videos/
                                 person:files-<person> 2770 (scrinium.md §9)
  .atrium/                       root:wheel 0755
    apps/<app id>/               the app's overlay volume (§3), app uid 0700
    workspaces/<name>/           a workspace's volumes (§5), workspace uid
    state/<component id>/        session components' state (forum.md §6)
```

- **No dotfiles at the top.** A shell's `.profile`, `.ssh`, `.cargo` and
  `.config` live in the home of the jail the shell runs in: an app's, a
  workspace's. Each jail has its own.
- **One tree per person.** Moving a person to another machine, backing
  them up or removing them is one directory. The policy file
  (`/var/db/atrium/<person>/policy.toml`) stays outside: it is
  authorization, kept by portcullisd, and travels with a backup as its own
  file.
- **Mode 0700 on the home is enough.** Every folder below it reaches a jail
  by a `nullfs` mount made by root, which needs no search permission on the
  path above it, so app uids never need to traverse the home.
- `.atrium` is root's so that nothing (the person's own uid included, should
  anything ever run as it) can swap an app's folder for a link.

## 3. Per-app data under the home

Today an app's persistent state is one overlay volume per app id at
`/var/lib/atrium/overlays/<id>` (portcullis.md §4.2): one per machine, not
per person, so two people would share an app's state. It moves to
`/home/<person>/.atrium/apps/<id>/`, still a quota'd Tessera volume mounted
there, still unioned over the app's read-only root in its jail. Each person
has their own; a person's app jail mounts only theirs.

**The app's uid, per person (decision H4).** With per-person overlays, the
same per-app uid would own files in two people's homes; isolation between
the two would rest on mounts alone. Recommended: portcullisd allocates the
per-app uid per (person, app), so the kernel separates them too. The uid
range (50000+) has room: 1000 people x 15000 apps.

**Migration.** At the first launch after the change, portcullisd moves an
existing overlay volume's mount from `/var/lib/atrium/overlays/<id>` to the
home of the person who launched it; on a machine with one person that is
everyone's. Session components' folders move from
`/var/db/atrium/state/<person>/` the same way. Components that run before
anyone logs in (Vestibulum) keep `/var/db/atrium/state/`.

## 4. App consoles

Right-click an app's icon (the dock, the Applications grid, the overview),
**Terminal**: a terminal window opens with a shell in that app's jail.

- **How.** Forum asks portcullisd (`Console { app id }`, with a token for
  the press, as Open with uses, opening-files.md §6.2). portcullisd starts
  the app's jail if it isn't running, or joins it if it is, and starts
  `/bin/sh` there as the app's uid on a new pty. The pty's master side is
  handed, as a descriptor, to the terminal app (`atrium-term`, its own jail,
  with no capabilities beyond graphics and the clipboard). The terminal
  never enters the app's jail; the shell never sees the terminal's.
- **What the shell can do** is what the app can: read and write the app's
  own files, use the app's network if it has one, run what is in the app's
  root. If the app's root has no shell or tools, portcullisd mounts a small
  read-only toolbox (`sh`, `ls`, `cat`, `cp`, `mv`, `rm`, `ps`, `top`, `vi`)
  at `/atrium/console`, first on `PATH`.
- **Not for every app.** System apps whose data is a secret or a
  capability (Arca, Sigillum, Vestibulum, Scrinium, dispensatord's clients)
  and session components have no console: their files are not the person's
  to edit by hand, and a console there would be a way around their own
  checks. Third-party apps always have one; the person owns that data
  anyway (decision H5).
- The window's title names the app ("Terminal: Atrium Player").
- **Kept like tmux (Stoa).** When stoad runs, portcullisd still starts
  every shell itself (jail, uid, toolbox) and hands it to stoad (`ADOPT`
  on stoad's root-only control socket: name, app, the process descriptor
  and the pty master). Each console is a Stoa session named
  `console.<person>:<app>:<n>`; the terminal gets a local link to it (a
  Unix datagram socketpair, `ATTACH`, with the session key), not a
  network port: its jail has no network. Closing the window detaches; the
  shells run on. **Terminal** on the app again reattaches the person's
  detached session, as it was. Without stoad the console is a bare pty
  that ends with its window.
- **Tabs and windows.** Ctrl+Shift+T asks portcullisd for another shell
  in the same session (a Stoa window, a tab); Ctrl+PageUp and
  Ctrl+PageDown move between them. Ctrl+Shift+N asks for another console
  window (a session of its own). The terminal holds the `consoles`
  capability, and portcullisd grants these only for an app the terminal
  already has a console of, for the same person, with the same press
  rules; it never names a jail or a uid itself.
- Console sessions are not saved to disk and stoad never starts their
  shells: a shell that exits is gone; when the last one goes, the session
  ends.
- Room left for personas (personas.md, proposal): the console runs in the
  jail of what it serves, so it inherits that instance's network and,
  later, its persona.

## 5. Workspaces

A workspace is a dev-jail (atrium-devjail.md) made into something one can
start from Forum: **New workspace** asks for a name and a toolchain (C,
Rust, Python, Go, Node, plain shell), creates the jail from the shared base
with that toolchain, and opens its terminal. A ready-made **Scratch**
workspace (C and shell) exists from the first login, for trying something
quickly.

- **Files.** A workspace's files are its own volumes under
  `/home/<person>/.atrium/workspaces/<name>/` (source, build, cache, as
  atrium-devjail.md §8). The source volume is mounted in the jail at
  `~/work`.
- **Its terminal** is a console (§4) on the workspace's jail; several can
  be open at once. Editors in their own jails reach a workspace over SSH,
  as atrium-devjail.md §1.1 already says.
- **Network** is declared when the workspace is made (atrium-devjail.md
  §3.5), defaulting to outbound only, and changed in its settings, not from
  inside it.
- **Files in and out.** Through Scrinium, like any app: a workspace sees
  none of the person's Documents unless a file or a folder is handed in
  with the picker. Scrinium shows each workspace's `work` folder under
  Workspaces, so results can be copied out.
- **`.atrium/devjail.toml`** in a project still reproduces a workspace from
  a repository (atrium-devjail.md §3); New workspace writes one.

**As built (W3, 2026-11-15).** portcullisd keeps workspaces
(`portcullisd/src/workspace.rs`). Each has a record and a writable root in
`~/.atrium/workspaces/<name>/.state/` (root's, 0700) and its files in
`~/.atrium/workspaces/<name>/work` (the person's, group `files-<person>`,
2770). Its jail (`ws_<person>_<name>`) is created with `persist`: the base
system read-only (`/bin`, `/lib`, `/libexec`, `/sbin`, `/usr/{bin,include,
lib,libdata,libexec,sbin,share}`), its own `/etc` (its accounts, made with
`pwd_mkdb`), `/tmp`, `/var`, `/usr/local` and `~/work` writable. It runs as
its own uid, allocated per person and workspace like an app's (registered
as `workspace/<name>`), which is in the person's files group, so the file
manager and the workspace can hand each other files; consoles use umask
002. Network `outbound` is a routed epair from jaild, `none` its own empty
stack. The terminal is a console (§4) with target `workspace/<name>`,
starting in `~/work`; Terminal starts a stopped workspace.
- Toolchains: C and the shell come with the base system; Rust, Python, Go
  and Node are listed but refused until their layers exist
  (atrium-devjail.md §4, atrium-pkg).
- Forum: the Applications grid shows each workspace as a tile that opens
  its terminal; typing a name no workspace has offers **New workspace
  "name"** (C, outbound), made and opened on the same press.
- **Workspaces app** (`atrium-workspaces`, org.atrium.workspaces, built
  2026-11-15): the person's workspaces in a list; New workspace (name,
  toolchain, network; Create and open on the same press); per workspace
  Open terminal, Stop, Network, Uses and Delete. It holds the restricted
  capability `workspaces` (system policy only), which lets it list, make,
  change, stop and delete its owner's workspaces and open their terminals.
  Delete asks first, then needs a press frescod confirms (activation
  purpose APPROVE), and is refused while another workspace uses it or
  anything is still mounted under it. Gate
  `scripts/vm-workspaces-app-check.sh` (clicks in the headless VM).
- Scrinium: a **Workspaces** place (`~/.atrium/workspaces` mounted in the
  file manager's jail); each folder shows its `work`, never `.state`.
- The record carries a persona, `personal`; links across personas are
  refused (personas.md, a proposal).

### 5.1 Several projects in one workspace

A workspace is a jail, not a project. Projects that grow together can live
side by side in one workspace's `work` and see each other as plain folders;
splitting one out later is a move into a new workspace.

### 5.2 Linking workspaces

Workspace B can **use** workspace A (B's settings, "Uses: A"): A's `work`
is mounted into B read-only at `/workspaces/A`, so B builds against A's
headers and links A's libraries; a rebuild in A is seen in B at once, and B
can't change A. All workspaces share the base system, so A's libraries link
in B. A **read-write** link is a separate choice, confirmed with a press.
Links are mounts: free, and gone when removed. For services rather than
files (a database in A that B talks to), B lists A's port and only that port
is open between them (atrium-devjail.md §3.6, §7).

### 5.3 Workspaces have no windows (proposal, 2026-11-15)

Shankar, 2026-11-15: every app is isolated anyway, so nothing needs to run
*in* a workspace to be safe. A workspace is a build box: compilers, sources,
build output, a shell. It has no display socket and never will. What it
builds runs as a regular app; the IDE that edits it is a regular app; the
terminal is a regular app. Spaces (Forum's desktops) are not tied to
workspaces.

- **Run what you built (W5).** In a workspace's project, `atrium.toml`
  sits beside the sources. **Run** (the Workspaces app, the IDE, or
  `portcullis workspace run <ws> <path>` from its console) asks
  portcullisd with a press. portcullisd reads the manifest and the binary
  from the workspace's `work` (never from inside the jail: it opens them
  from the host side, refuses links out of `work`), copies them into a
  **dev bundle** at `~/.atrium/dev/<ws>/<id>/`, and launches it as the
  app `dev.<ws>.<id>`: its own uid per (person, app) as for any app, its
  own overlay, exactly the capabilities its manifest declares, through
  the same prompts as an installed app. Differences from an installed
  app:
  - unsigned, never in the Applications grid, not started at login;
  - no restricted capabilities (system-policy-only ones are refused, not
    prompted), and no grant is remembered past the run unless the person
    picks "keep" in the prompt;
  - its windows carry a **dev** mark on their chip and frame, with the
    workspace's name, so a dev build is never mistaken for the real app;
  - Run again replaces the bundle; deleting the workspace deletes its dev
    bundles and their data.
  A dev bundle can be installed for real only through Opifex with a
  signature, as today.
- **The IDE is a regular app (W6).** It holds a new capability
  `workspace-exec`: it may list the person's workspaces and, after the
  person picks one in it (a press), get that workspace's `work` mounted
  into its jail read-write at `/work/<name>` and ask portcullisd to run a
  command in the workspace (`WorkspaceExec { name, argv, cwd }`): the
  process runs in the workspace's jail as the workspace's uid, its output
  comes back on a pipe, and it can be stopped. The IDE never gets a shell
  of its own and never names a jail or a uid. Build, test and Run go
  through this; a language server runs the same way and talks over its
  pipe. The IDE only reaches the workspaces the person picked in it.
- **Terminal is a regular app (W7).** Terminal appears in the Applications
  grid. Opened on its own, a tab is a shell in an **empty jail**: the
  read-only toolbox, a private `/tmp`, no home, no network, no app's files,
  its own uid. A switcher at the top of the window ("Empty ▾") moves the
  tab into a workspace or an app's console (§4): portcullisd asks for a
  press and hands over a shell in that jail, as today. Open terminal in
  the Workspaces app and Terminal on an app icon open it already
  switched. Each tab names where it is; Stoa keeps them as today. This
  replaces H6's "no general terminal": there is one, and it reaches
  nothing until the person switches it.

### 5.4 Not rebuilding everything (proposal, 2026-11-15)

Big projects carry large build output and caches; moving work to another
workspace must not mean rebuilding from nothing.

- **Uses links** (§5.2) already let B build against A's output where it
  is, read-only: A's `target/` or `build/` is never rebuilt in B.
- **No shared download cache (Shankar, 2026-11-15).** Each workspace
  downloads its own crates, modules, packages and distfiles into its own
  root. Tessera dedups the copies on disk, so sharing would save only the
  download itself, and a shared writable cache would let one workspace
  hand another a changed package (cargo, Go and pip trust a cache once
  it's filled; a port built from local or patched sources checks nothing).
- **Compiled-output cache, by group, opt-in (W9).** ccache/sccache-style
  objects can't be checked by hash, so a compromised workspace could plant
  a bad object. They are shared only within a group of workspaces linked
  by uses (they already trust each other's output), at
  `~/.atrium/cache/objects/<group>`; off by default, turned on in the
  Workspaces app per workspace.
- **Duplicate workspace (W8).** A copy-on-write clone of the workspace
  (its `work` and its root, a Tessera snapshot; a plain copy where the
  volume isn't Tessera) brings its build output along: splitting a
  project out or trying something risky costs no rebuild. The copy gets
  its own uid; files are re-owned as they are cloned.

## 6. The host shell

Root's shell is for administration and recovery: Curia's Recovery page opens
one after a Sigillum check (a FIDO2 key or a fingerprint, as for Arca's
unlock) of a person with the administrator role, in a window marked as
such. During development we keep SSH to the host as today; images for
people ship without `sshd` on the host.

## 7. Decisions

| | decision | recommendation |
|---|---|---|
| H4 | per-app uid per app, or per (person, app) | per (person, app) |
| H5 | which apps have a console | every third-party app; no system app or session component |
| H6 | a general terminal | none; workspaces and app consoles instead, the host shell for administration only |
| H7 | where per-app data lives | `/home/<person>/.atrium/apps/<id>`, one overlay volume per person and app |
| H8 | where workspace output runs | as a regular dev app (§5.3), not in the workspace; workspaces have no windows (Shankar 2026-11-15) |
| H9 | dev app grants | asked per run, restricted capabilities refused, "keep" to remember |
| H10 | the IDE's reach | `workspace-exec`: only workspaces the person picked in it, commands run in the workspace's jail |
| H11 | Terminal | a regular app; alone it is an empty jail, switched into a workspace or app console by a press (Shankar 2026-11-15) |
| H12 | caches across workspaces | no shared download cache (Tessera dedups the copies), compiled cache only within a uses group and opt-in, Duplicate workspace by copy-on-write (Shankar 2026-11-15) |

(H1–H3 are scrinium.md §9's.)

## 8. Build steps

1. **W1 the home.** First login makes the folders and `.atrium` (with
   scrinium.md §9's H1: the files group and `home-files`); portcullisd
   mounts overlays from the home and allocates per-(person, app) uids;
   migration from `/var/lib/atrium/overlays` and `/var/db/atrium/state`.
   Gate: two people, one app, neither sees the other's state; a moved
   overlay keeps its contents.
2. **W2 app consoles.** portcullisd `Console`, the pty handed to
   `atrium-term`, the toolbox mount, Forum's menu item. Gate: a console in
   an app's jail sees its overlay and nothing else, has the app's uid, and
   is refused for Arca.
   **Built 2026-11-15**: the bare-pty console (47974e49), then
   Stoa-backed consoles with tabs, windows, detach and reattach;
   `scripts/vm-console-check.sh` 0 failed (33 checks).
3. **W3 workspaces.** New workspace and Scratch in Forum, on
   `atrium-devjail`; Scrinium's Workspaces place. Gate: `cc hello.c && ./a.out`
   in Scratch; a second workspace using the first links its library.
   **Built 2026-11-15** (§5 "As built"); `scripts/vm-workspace-check.sh`.
4. **W4** Scrinium's H2 and H3 (picker over home, bookmarks) on the new
   layout.
5. **W5 Run.** Dev bundles, `dev.<ws>.<id>` launch, dev mark in frescod's
   frame, Run in the Workspaces app. Gate: a hello-window app built with
   `cc` in Scratch runs as its own uid, can't read the workspace, and a
   manifest asking for a restricted capability is refused.
   **Built 2026-11-15** (`portcullisd/src/dev.rs`). Differences from the
   proposal above: the id is `dev.<person>.<workspace>.<id>` (two people's
   Scratch builds are two apps), and the bundle is installed with the
   others under `/var/lib/atrium/apps` so every launch path applies, not
   in the home. `WorkspaceRun { name, project, activation }`: the
   manifest and entry are read through descriptors opened
   `O_RESOLVE_BENEATH` the project folder, itself beneath `work` (a link
   out is ENOTCAPABLE); system-only and restricted capabilities refuse
   the Run before anything is installed; the entry's libraries and the
   loader are copied from the base system (its ELF is parsed, never run;
   a library from the workspace's `/usr/local` isn't carried yet); the
   bundle is signed by this machine's **dev publisher**
   (`/var/db/atrium/dev-publisher`, made at the first Run, private key
   root's 0600), which portcullis-trust accepts for `dev.` ids alone and
   for nothing else, so a publisher's signature doesn't make a dev build;
   the Run press (APPROVE, from an app; none from the person's own CLI,
   `portcullis workspace run <ws> [folder]`) grants what the manifest
   declares, for that manifest's hash; the app starts. A running build is
   not replaced (close it first). Dev ids are left out of the catalog.
   frescod frames every window of a `dev.` app in amber with the tag "dev
   build: <id> from <workspace>" (the sheets' frame code), and reads the
   launch registry again for a uid it doesn't know yet. Deleting the
   workspace removes its builds and their grants (refused while one
   runs). The Workspaces app has a "Run a build" field and button. Gate
   `scripts/vm-run-check.sh` 0 failed (cc -static build, uid and home
   checks, refusals, forged dev id, Run on the button with the amber
   frame, delete).
6. **W6 `workspace-exec`.** The capability, `WorkspaceExec`, the pick.
   Gate: a test app builds in a picked workspace and is refused for one it
   didn't pick.
7. **W7 Terminal app.** Empty-jail shell, the switcher, the grid entry.
   Gate: an empty tab sees no home and no network; switching to Scratch
   needs the press and lands in `~/work`.
   **Built 2026-11-15**: portcullisd's console target `empty`
   (`portcullisd/src/empty.rs`): a persistent jail `empty_<person>` whose
   root, made again at each start, holds only the toolbox, `/dev`, `/etc`
   (its two accounts), `/root` and `/tmp`; vnet with nothing but `lo0`;
   its own uid (registered `console/empty`). It needs no press, and the
   terminal (`consoles`) may ask for it any time; Terminal opened alone
   asks for it after 1.5 s with no console on its way. The strip at the
   top of each window names where the shell is; a click on it or
   Ctrl+Shift+O opens the switcher (Empty and the person's workspaces;
   app consoles still come from an app's icon), and a press on a row or
   Enter asks portcullisd for that console with `tab_of` none and not a
   new window, which takes the press like a launcher's and reattaches a
   detached session; it replaces the window's shell, the old one detaching.
   Gate `scripts/vm-terminal-check.sh` 0 failed.
8. **W8 Duplicate.** Gate: a duplicate keeps the build output and the
   root's tools, re-owned to its own uid; the original is unchanged. (The
   shared download cache was dropped, Shankar 2026-11-15.)
   Built 2026-11-15: `portcullis workspace duplicate <from> <to>` and the
   Workspaces app's "Duplicate…"; `cp -Rp` of `work` and the root (links
   as links), what the old uid or group owned re-owned to the new, the
   home in the root renamed. Gate `scripts/vm-duplicate-check.sh` 0 failed.
9. **W9 group object cache.** Gate: a workspace outside the group can't
   read or write the group's objects.
