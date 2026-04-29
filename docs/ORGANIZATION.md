# Repository organization

For component naming (Atrium, Fresco, Tessera, Portcullis, Castellum, ...) see [NAMING.md](NAMING.md).

## Principles

1. **Things that version together live together.** Anything that has to change in lockstep with the wire protocol goes in one repo.
2. **Things with different release cadences split.** Kernel modules ship with kernel updates; apps ship independently; spec ships rarely.
3. **External contributors should be able to focus.** A kernel hacker shouldn't wade through Rust apps; an app developer shouldn't need to understand the kmod build.
4. **Distribution is via ports (and eventually Opifex).** Each shipping component eventually gets a freebsd-ports entry. Source repos publish releases; ports tree consumes.
5. **The spec stands alone.** Vendors implementing Fresco shouldn't need to read our server's source.

## GitHub layout

A single GitHub organization, **`atrium-os`**, with the following repos. Tiered by who needs to touch each.

### Tier 1 — Platform core (monorepo)

**`atrium-os/fresco`** — the Fresco protocol implementation. Versions together because everything here speaks the wire protocol.

```
fresco/
├── server/                  Rust: fresco display server
├── libfresco/               C:   userspace client library
├── fresco-rs/               Rust: safe bindings + high-level API
├── fresco-text/             Rust: text shaping / glyph atlas
├── examples/                small smoke-test apps (hello_rect, multi_client)
└── vendor/                  pinned third-party (where unavoidable)
```

**Rationale:** the server, both client libraries, and test apps **must** ship together because a protocol change touches all of them. Single PR moves all of them; CI validates the whole thing.

**`atrium-os/atrium`** — platform integration umbrella: top-level docs, rc.d scripts, default config, `atrium-info` CLI, anything that ties the components together but isn't itself a component.

### Tier 2 — Spec & reference

**`atrium-os/fresco-spec`** — the wire-format spec, broken out as a clean reference for vendors / standards bodies. Initially mirrors `fresco/proto`-style content; once D7 begins, it becomes the maintained spec with versioning policy and conformance suite.

**Rationale:** vendors implementing Fresco against future scenegraph hardware shouldn't have to clone our entire platform repo. They want the spec, full stop.

### Tier 3 — Kernel land

**`atrium-os/atrium-kmod`** — FreeBSD kernel modules.

```
atrium-kmod/
├── transport/               /dev/fresco0 transport cdev (today's fresco.ko)
├── gpu/
│    ├── virtio/             atrium-virtio-gpu (D0)
│    ├── mali/               atrium-mali (later)
│    ├── amd/                atrium-amdgpu (later)
│    └── ...
├── display/                 /dev/atrium-display0 (modesetting)
├── input/                   HID via usbhid/hkbd; no evdev
├── tessera/                 in-kernel CAS-FS (D1.5; if not FUSE)
└── shared/                  shared kernel-side helpers
```

**Rationale:**
- Kernel modules have different ABI concerns (must match running kernel version).
- Different contributor pool — kernel hackers vs. app developers.
- Build system tied to FreeBSD kernel source tree (`bsd.kmod.mk`).
- Release cadence tied to FreeBSD release-engineering, not protocol revisions.

Cross-repo coordination with `fresco`: a versioned `atrium-os/fresco-spec` reference (git submodule or a published header package) so the kmod's wire-format constants stay in sync.

### Tier 4 — System services (per-service repos)

System services that aren't graphics. Each is its own small server, talking over Castellum (the IPC bus).

- **`atrium-os/portcullis`** — jail launcher + capability manifest reader.
- **`atrium-os/castellum`** — IPC bus library + reference daemon (`castellumd`).
- **`atrium-os/vestibulum`** — display manager / login.
- **`atrium-os/lyra`** — audio server (per-client streams, content-addressed sample buffers).
- **`atrium-os/tabula`** — clipboard service (CAS-blob-hash + format declaration).
- **`atrium-os/praeco`** — notification daemon.
- **`atrium-os/curia`** — settings store + control panel.
- **`atrium-os/scrinium`** — file picker / browser service.
- **`atrium-os/opifex`** — package fetch / verify / install / rollback.
- **`atrium-os/forum`** — shell: wallpaper + statusbar + dock.
- **`atrium-os/tessera`** — CAS-FS userspace tooling (`tessera import`, `tessera gc`, etc.) and FUSE driver if applicable.

**Rationale:** each is a small, focused service. Contributors care about audio OR clipboard, not both. Independent release cadence.

### Tier 5 — Foundation apps (per-app repos)

User-facing apps. Each builds independently, each gets its own port. Plain descriptive names with `atrium-` prefix.

- **`atrium-os/atrium-edit`** — text editor.
- **`atrium-os/atrium-term`** — terminal emulator.
- **`atrium-os/atrium-files`** — file manager (built atop Scrinium).
- **`atrium-os/atrium-image`** — image viewer.
- **`atrium-os/atrium-pdf`** — PDF viewer.
- **`atrium-os/atrium-clock`** — clock widget.
- ...

Plus eventually:
- **`atrium-os/servo-fresco`** — Servo with Fresco backend (or a fork).
- **`atrium-os/slint-fresco`** — Slint backend integration.

### Tier 6 — Ports tree fork

**`atrium-os/freebsd-ports`** (fork of `freebsd/freebsd-ports`).

Each releaseable component gets a port:

```
ports/
├── graphics/
│    ├── fresco-server/        Makefile pointing at atrium-os/fresco
│    ├── libfresco/
│    ├── atrium-virtio-gpu/    (kmod)
│    ├── atrium-mali/          (kmod, when ready)
│    └── ...
├── editors/
│    └── atrium-edit/
├── atrium/                    (new category)
│    ├── forum/
│    ├── vestibulum/
│    ├── portcullis/
│    └── ...
├── audio/
│    └── lyra/
└── ...
```

`pkg install atrium` Just Works for end users (meta-port pulling in the platform).

**Migration to upstream:** once Atrium has critical mass (D5+) and quality, these ports get upstreamed to `freebsd/freebsd-ports` proper. Until then, users add our fork as an additional ports tree.

## Repo dependency graph

```
fresco-spec
   ▲
   │ (versioned reference)
   │
fresco ──────┬──► fresco-rs / libfresco (consumed by everything below)
             │
             ▼
       atrium-kmod
             │
             ▼
   ┌─────────┴───────┬──────────┬──────────┬──────────┬──────────┐
   │                 │          │          │          │          │
portcullis      castellum     lyra      tabula     praeco     opifex
   │                 │          │          │          │          │
   └────────┬────────┴──────────┴──────────┴──────────┴──────────┘
            ▼
      forum, vestibulum, curia, scrinium, tessera tooling
                            │
                            ▼
                  foundation apps (atrium-edit, atrium-term, ...)
                            │
                            ▼
                     freebsd-ports fork
```

## Versioning

- **Wire protocol** (`fresco-spec`) — semver: `MAJOR.MINOR.PATCH`. Major = wire-incompatible. Minor = additive (new opcode). Patch = clarification only.
- **Server, libfresco, fresco-rs** — match wire protocol minor; ship from `fresco/` together.
- **kmod** — its own version, declares supported wire-protocol range (e.g. `kmod 1.4.x supports wire 1.0..1.3`).
- **Apps** — independent semver. Each declares minimum libfresco version.
- **Services** — independent semver. Each declares minimum libfresco / libcastellum version.

CI gate: any PR touching the wire-format spec requires a matching version bump and updates to all in-tree consumers.

## Castellum: replacing D-Bus

D-Bus serves three functions on Linux:
1. **System services discovery** (find the audio daemon, find the network manager).
2. **Method dispatch** (call audio.SetVolume(0.5)).
3. **Signals / events** (audio.VolumeChanged).

For Atrium, we want a system that's:
- Capability-aware (jails restrict who can talk to whom — gated by `atrium.toml` manifest).
- Content-addressed (large payloads via hash, not race-prone temp paths).
- Per-client ring isolated (consistent with Fresco's pattern).
- Doesn't require an XML interface description language or runtime introspection.

### Castellum

A meta-protocol modeled on Fresco itself. Each service listens on a well-known socket (`/var/run/atrium/<service>d.sock`) and/or a cdev for shared-memory cases.

Each service:
- Implements its own opcode set (Lyra: open_stream, write_samples, set_volume; Tabula: put, get, list_formats).
- Per-client slot rings, just like fresco-server (cmd / comp / event), where shared memory matters.
- Uses content-addressed payloads where applicable (clipboard data, audio samples, large config blobs).

**`castellum` (the repo) provides:**
- A small library (`libcastellum`) with the per-slot ring + socket plumbing, reusable by every service.
- Specs for service registration, capability-gated access, and event subscription.
- A reference simple service for examples.
- `castellumd` — the bus admin daemon for diagnostics and policy.

Services that don't need shared memory (small RPCs, infrequent calls) use a simpler request/response over the Unix socket, also speced here.

This is **lighter than D-Bus** (no central daemon required for the data path, no XML interfaces, no runtime introspection), **stronger** (capability-gated by jail manifest), and **consistent** (every system service has the same shape as the graphics server).

CLI tool: `castellum list-services`, `castellum capabilities <service>` for diagnostics.

## Migration from current layout

Current state:

```
~/src/bsd/                ~/src/fresco-server/
├── atrium-clock/         (separate; Rust)
├── atrium-edit/
├── atrium-find/
├── atrium-term/
├── fresco-kmod/
├── fresco-rs/
├── fresco-text/
├── libfresco/
├── scripts/
├── vm/
└── docs/
```

## Source layout vs distribution

Two orthogonal axes, easy to conflate:

- **Source layout** — monorepo vs per-component repos. About how we develop.
- **Distribution** — ports, Opifex/Tessera jail trees, tarballs. About how users install.

A port is a Makefile in `freebsd-ports` pointing at *some* source URL. The source can be a subdirectory of a monorepo or a standalone repo — ports doesn't care. So "apps should be ports" (true) does not force "apps must be per-repo" (not necessarily true yet).

**Distribution endgame:** apps ship as jail trees in Tessera, delivered by Opifex. Ports is the bridge during bring-up so `pkg install atrium-edit` works on stock FreeBSD.

## Current stance: monorepo through D3

The tiered split above is the long-term north star, **not the v0 plan**. Until ~D3 we keep `atrium-os/atrium` as a working monorepo (kmod, apps, gpu binding, socket lib, compositor) plus `atrium-os/fresco` as the protocol tree. Reasons:

- Wire protocol is unstable — single opcode changes touch 4+ components in one PR; cross-repo coordination is pure friction.
- One contributor. The "different contributor pools" argument is hypothetical until those people exist.
- Apps are small (atrium-edit-socket is a few hundred lines). Per-app CI + release is more ceremony than code.
- Atomic refactors are still common. Splits would convert each one into a multi-repo dance.

**Split triggers** (revisit when any become true):

- Wire format stabilizes — minor versions add opcodes, majors are rare.
- A real second contributor appears who only cares about one component.
- An app grows past ~5k LOC with its own release cadence.
- A vendor wants `fresco-spec` without cloning the platform.
- `freebsd-ports` upstreaming starts (forces per-component release tarballs).

Likely milestone: right before the first external release. By then real seams will be visible vs imagined.

## Migration plan (deferred)

To be executed when the split triggers above fire — NOT now:

1. **Move `fresco-server` into `~/src/bsd/server/`**, then push the relevant subset (server, libfresco, fresco-rs, fresco-text, examples) as `atrium-os/fresco`.
2. **Push `fresco-kmod/`** as `atrium-os/atrium-kmod`. Add a top-level Makefile that builds all kmod targets.
3. **Push apps individually** as `atrium-os/atrium-edit`, `atrium-term`, `atrium-clock`, `atrium-find`.
4. **Push `freebsd-ports` fork** with port files for all of the above.
5. **Stand up `atrium-os/fresco-spec`** with the protocol reference doc and the conformance test vectors.
6. **Stand up `atrium-os/atrium`** (umbrella) with the docs that currently live in `~/src/bsd/docs/`.

Until that happens, the local monolithic layout is fine — it accelerates iteration during pre-publish development.

## What goes in the README of each repo

Cross-cut consistency:

- Top of each README: "Part of the Atrium platform — see [atrium-os/atrium](https://github.com/atrium-os/atrium)."
- "What this is" — one paragraph.
- "How to build" — make/cargo invocations.
- "How to test" — basic smoke test.
- "What this is not" — explicit non-goals (e.g. "not a Wayland implementation; not a Linux compat layer").
- License (probably BSD-2-Clause or BSD-3-Clause to match FreeBSD's tradition).

## Branch / release strategy

- `main` — stable. CI green. Tagged releases.
- `develop` — active development. Merges to main via PR.
- Per-feature branches off `develop`.
- Releases are signed git tags + (where applicable) tarballs uploaded to GitHub Releases.
- Each repo has CI: build + test + lint. Cross-repo CI for spec changes is harder; for now, manual coordination, automate later.

## License

Probably **BSD-2-Clause** or **BSD-3-Clause** uniformly:

- Matches FreeBSD's tradition.
- Permissive — encourages vendor adoption (a hardware vendor implementing Fresco can do so without GPL contagion).
- Compatible with the Apache-2.0 / MIT crates we depend on.

Some kernel modules may need to dual-license against GPL-compatible if they incorporate any GPL'd code (unlikely given our "no linuxkpi" stance, but worth declaring policy).

## Open questions

- **Org name.** `atrium-os` is the working choice. Reserve early.
- **Slack/Matrix/IRC for contributors?** Eventually. Not a v0 concern.
- **Mailing list?** FreeBSD culture leans on mailing lists. Probably useful.
- **Code-review tool.** GitHub PRs are fine. Consider Gerrit if scale demands.
- **Trademark.** "Atrium" and "Fresco" are both generic enough that someone may already own competing trademarks. Worth checking.
- **Domain.** `atrium-os.org`? `atrium.dev`? Lock down.
