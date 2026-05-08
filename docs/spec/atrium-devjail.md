# atrium-devjail — per-project developer environments

**Status:** design, 2026-05-08.
**Owner:** D2.5 dev-experience track (consumes portcullis, storage,
network, atrium-volumes infrastructure already spec'd).

How an Atrium user creates and works in a per-project development
environment. The dev-jail is one FreeBSD jail per project,
preinstalled with the project's toolchain, with the project's
files on a Tessera volume, accessed via SSH from the user's IDE
jail (or terminal). The mechanism that makes "I can hack on
five projects today, each with its own Rust nightly / Node 18 /
Python 3.12 / Go 1.21, none of them touching the host or each
other" routine.

> **Position relative to other docs.** This spec is the consumer.
> It pulls together:
> - [`portcullis.md`](portcullis.md) — capability manifest
> - [`storage.md`](storage.md) + [`atrium-volumes.md`](atrium-volumes.md) — project file storage
> - [`tessera-quotas.md`](tessera-quotas.md) — per-project disk limits
> - [`network.md`](network.md) — inter-jail SSH networking
> - [`atrium-netd.md`](atrium-netd.md) — pf rule lifecycle
> - [`portcullis.md`](portcullis.md) §3.4 — first-run setup phase
> - future `atrium-pkg.md` — per-jail toolchain installation
> Every infrastructural decision needed to ship dev-jails has
> already been made in those docs. This one specifies the user-
> facing surface.

## 1. The premise

"Every app is jailed, including your editor" is structurally
correct but operationally insufficient if the developer flow is
awkward. dev-jails are the feature that makes Atrium a viable
working environment for Rust-on-FreeBSD-and-everything-else
development:

- **One project, one jail.** Each project on disk corresponds
  to one Atrium dev-jail. The project's source files, build
  artifacts, language toolchain, and any DB/service the
  project needs — all live inside that jail.
- **Per-project toolchain isolation.** Project A uses Rust
  1.83; Project B uses Rust nightly + a custom LLVM. They
  install in their own jails; no host pollution; no `rustup`
  weirdness; no version-conflict swearing.
- **SSH is the access pattern.** VS Code (jailed; see §1.1),
  the user's terminal (jailed; in their session jail), any
  other IDE — all reach the dev-jail via SSH on a lo0 alias.
  No filesystem bind mounts; no host-FS escape paths;
  no uid mapping awkwardness.
- **Cheap to create, cheap to destroy.** A dev-jail's rootfs
  is a Tessera CAS overlay; the actual unique data is the
  project + the toolchain configuration. Creating "yet another
  experimental dev-jail" costs ~30s and a few hundred MB.
- **The project's manifest is in the project's git.**
  `.atrium/devjail.toml` lives alongside `Cargo.toml`,
  `package.json`, `pyproject.toml`. Cloning the repo and
  running `atrium-devjail create` reproduces the environment.

### 1.1 Why VS Code being jailed cleans this up

Pre-Atrium, "VS Code accesses the dev container via Docker
volume + uid mapping" is a pile of Linux-shaped duct tape. In
Atrium, **VS Code is its own jail too** (with capabilities for
graphics, clipboard, network, your `~/code/`). It can't
directly see the dev-jail's filesystem; it has to go through a
network protocol. SSH is that protocol — well-understood,
debuggable, and exactly what VS Code Remote SSH is for.

This isn't a workaround; it's the cleanest solution for an
"everything is jailed" platform. Linux dev containers
contort themselves to give the IDE filesystem access to the
container; Atrium's IDE-jailed-too premise removes the contortion.

## 2. The user journey

```
$ cd ~/code/myproject
$ ls -la
.atrium/                 # dev-jail manifest lives here
.git/
Cargo.toml
src/
README.md

$ cat .atrium/devjail.toml
[devjail]
name = "myproject"

[toolchain]
language = "rust"
rust_version = "1.83"

[packages]
freebsd  = ["pkgconf", "openssl"]
cargo    = ["cargo-watch"]

[volumes]
source.size_max  = "20G"      # project files
build.size_max   = "10G"      # target/
cache.size_max   = "5G"       # ~/.cargo, sccache

[network]
outbound = ["github.com:443", "crates.io:443",
            "static.crates.io:443", "registry.atrium.dev:443"]

[setup]
script = """
cargo fetch || true   # warm the cache; ok if no internet yet
"""

$ atrium-devjail create
  → reads .atrium/devjail.toml
  → asks atrium-pkg for the rust-1.83 + freebsd-deps overlay
  → asks atrium-volumes to provision source/build/cache volumes
  → asks portcullisd to launch the jail with SSH keypair injected
  → bridge IP allocated: 127.10.0.42
  → local /etc/hosts entry (in user's session jail) added:
       dev-myproject  127.10.0.42

  Dev jail "dev-myproject" ready.
  SSH:    ssh dev-myproject
  Status: atrium-devjail status

$ atrium-devjail ssh                          # interactive shell
dev-myproject% which rustc
/usr/local/bin/rustc
dev-myproject% rustc --version
rustc 1.83.0 (stable)
dev-myproject% cargo build
   Compiling myproject v0.1.0 (/atrium/source)
   ...

$ # OR launch VS Code (which is itself a jailed app):
$ atrium-launch vscode
  → VS Code starts in vscode-jail
  → user clicks "Connect to Host" → "dev-myproject"
  → VS Code Remote installs its server in the dev-jail (via SSH)
  → editor + terminal + LSP + debugger all run inside dev-myproject
  → file edits, builds, tests — all inside the jail
```

Project-relative files live in the dev-jail. The user's session
jail and vscode jail both **reach** the project via SSH; neither
*owns* it. Tests, builds, and tools run inside the dev-jail's
toolchain. None of this touches the host filesystem.

## 3. The `.atrium/devjail.toml` schema

Project-local file checked into the project's git, alongside
`Cargo.toml` / `package.json` / `pyproject.toml` etc. Cloning
the repo + `atrium-devjail create` reproduces the full
environment.

### 3.1 Top-level

```toml
[devjail]
# Name suffix; full jail name will be "dev-<name>".
# Defaults to the directory name (sanitized) if omitted.
name = "myproject"

# Optional human description.
description = "Atrium platform main repo"
```

### 3.2 Toolchain

The toolchain block declares the language environment. Drives
which package overlay atrium-pkg pulls in:

```toml
[toolchain]
# One of: rust | node | python | go | cpp | shell
language = "rust"

# Per-language version pin
rust_version    = "1.83"        # for language = "rust"
node_version    = "20.10.0"     # for language = "node"
python_version  = "3.12"        # for language = "python"
go_version      = "1.21.5"      # for language = "go"

# Multi-language projects: array form
languages = ["rust", "node"]    # alternative; pulls both overlays
```

Toolchain pinning makes the project reproducible across users
and machines. atrium-pkg resolves "rust 1.83" to a specific
content-addressed overlay tree; the same hash on every machine.

### 3.3 Packages

Beyond the toolchain, project-specific packages from FreeBSD's
ports tree or from the language ecosystem:

```toml
[packages]
freebsd  = ["pkgconf", "openssl", "libpq"]      # pkg(8) inside the jail
cargo    = ["cargo-watch", "cargo-audit"]       # `cargo install`
npm      = ["@biomejs/biome"]                   # `npm install -g`
pip      = ["black", "ruff"]                    # `pip install`
go       = ["github.com/air-verse/air@latest"]  # `go install`
```

These run during the `[setup]` phase (§3.7).

### 3.4 Volumes

Per-project storage. `atrium-volumes` provisions Tessera-backed
volumes; `tessera-quotas` enforces size limits. Manifest declares
what the project needs:

```toml
[volumes.source]
size_max  = "20G"             # project files (git repo)
mount_at  = "/atrium/source"  # inside the jail; this is where
                              # SSH lands by default

[volumes.build]
size_max  = "10G"             # build artifacts: target/, dist/,
                              # node_modules/
mount_at  = "/atrium/build"

[volumes.cache]
size_max  = "5G"              # toolchain caches: ~/.cargo,
                              # ~/.npm, sccache, etc.
mount_at  = "/atrium/cache"
```

Why split source / build / cache into separate volumes:

- **Different lifecycles.** Source = preserved aggressively;
  build = often blown away; cache = shared with other projects
  if compatible.
- **Different sizes.** Source: small (<1G). Build: medium-large
  (depends on language; Rust target/ can hit 10G+). Cache:
  shared and can be huge.
- **Different snapshots.** Snapshot policies can differ:
  source snapshots hourly, build never (transient).
- **Cross-project sharing for cache.** A future feature: the
  `cache` volume could be shared across dev-jails of the same
  toolchain (one Rust 1.83 cache for all your Rust projects).

Defaults if a volume is omitted: source = 20G, build = 10G,
cache = 5G. Operator policy in `jaild.policy.toml` may cap
the maximum any single project can request.

### 3.5 Network capabilities

Maps directly to `network.md`'s capability schema. Defaults are
"jail can do dev work":

```toml
[network]
outbound = ["github.com:443", "crates.io:443",
            "static.crates.io:443", "registry.atrium.dev:443"]
inbound  = [22]               # sshd; auto-included if not set

# Default if [network] section omitted entirely:
#   outbound = "any"     (most permissive; user-friendly default)
#   inbound  = [22]      (sshd reachable via lo0 alias)
```

The `outbound` field is the project author's chance to
*restrict* network access from CI-style "this project should
only need to reach a few hostnames" to default-permissive. If
omitted, the dev-jail can reach anywhere — that's the user-
friendly default; restrict in projects where supply chain
hygiene matters.

`peer_jails` is auto-set:

```toml
# Auto-generated; manifest can override:
peer_jails = []   # dev-jails don't need to dial other jails
```

The IDE jail (vscode) talks to the dev-jail, not vice versa.
The dev-jail's manifest doesn't need `peer_jails` unless the
project itself needs to talk to another Atrium service (e.g., a
project depending on `atrium-postgres-jail` would list it).

### 3.6 Service dependencies

For projects that need running services (Postgres, Redis,
Elastic, MinIO, etc.):

```toml
[[services]]
name  = "postgres"
image = "atrium-postgres:16"        # an atrium-pkg-published image
volumes = ["data:5G"]               # Postgres data dir
ports   = [5432]                    # listens on port 5432 of its alias

[[services]]
name  = "redis"
image = "atrium-redis:7"
ports = [6379]
```

Services are *separate* jails managed alongside the dev-jail.
Each gets its own manifest derived from the image's metadata.
The dev-jail's `peer_jails` is auto-extended with the service
names; the dev-jail can connect to them on their listed ports
via lo0.

`atrium-devjail create` provisions all of: dev-jail + services
together. `atrium-devjail destroy` tears them down together.

### 3.7 Setup phase

A first-run script that runs once per dev-jail (sentinel-based,
per `portcullis.md` §3.4 first-run setup):

```toml
[setup]
script = """
# Project-specific bootstrap. Runs once per dev-jail creation.
cargo fetch
( cd dev-tools && cargo build )
"""

# Optional: which network destinations the setup script may reach.
# If unset, inherits [network] outbound. Useful for tighter
# whitelisting during one-shot bootstrap.
network_outbound = ["github.com:443", "crates.io:443"]
```

The script runs as the jail's exec uid (default: a non-root uid
allocated by atrium-pkg per the toolchain's conventions). It
runs after toolchain + packages are installed; output goes to
`atrium-devjail logs setup`.

If the script fails (non-zero exit), the jail is marked
"setup-failed" and the user sees the error; the jail isn't
ready to use until they fix and re-run setup.

### 3.8 IDE / VS Code hints

Optional: hints that help VS Code Remote autoconfigure:

```toml
[ide]
default_path = "/atrium/source"      # where SSH sessions land

[ide.vscode]
extensions = ["rust-lang.rust-analyzer", "vadimcn.vscode-lldb"]
                                     # auto-installed in VS Code Server
                                     # inside the dev-jail on first SSH
```

VS Code Remote reads these to bootstrap the editor experience.
Other IDEs (JetBrains Gateway, etc.) read their own equivalents
or just SSH in raw.

## 4. The dev-jail rootfs (atrium-dev-base)

Each dev-jail starts from the `atrium-dev-base` rootfs — a
Tessera-backed minimal FreeBSD environment with:

- `sshd` preinstalled, configured to listen on the jail's lo0
  alias, key-only auth, no host keys reused across jails
- `pkg`, `tar`, `unzip`, `git`, `curl`, `bash` (operator's
  preference), `tmux`, `vim`/`nano`
- `ca-certificates` pre-loaded
- `atrium-devjail-agent` — a small daemon that exposes
  jail-side hooks for `atrium-devjail` (status, logs, restart,
  etc.) without granting SSH to the user's session
- Nothing else — no compilers, no language runtimes; those come
  from the toolchain overlay (§3.2)

Size: ~150 MB. Mostly base FreeBSD userland + sshd.

The toolchain overlay (Rust 1.83, Node 20, etc.) is a separate
Tessera CAS layer that mounts on top. CAS dedup means: if you
have ten dev-jails all using Rust 1.83, the rust-1.83 layer is
stored once and shared.

### 4.1 Layered filesystem

The dev-jail's `/` is an overlay:

```
/                                     ← dev-jail root (writable)
   ├── (atrium-dev-base CAS layer)    ← read-only Tessera CAS
   ├── (rust-1.83 toolchain layer)    ← read-only Tessera CAS
   ├── (project packages layer)       ← per-jail; from setup
   └── /atrium/source                  ← rw; project files volume
       /atrium/build                   ← rw; build artifacts volume
       /atrium/cache                   ← rw; toolchain cache volume
```

Per `storage.md`, the layered model is Tessera-overlay with the
project volumes mounted on top. CAS dedup means base + toolchain
are nearly free across multiple dev-jails sharing the same
versions.

### 4.2 SSH key management

When `atrium-devjail create` runs, it:

1. Reads the user's `~/.ssh/atrium_dev_id.pub` (default; or a
   path configured in `~/.config/atrium/devjail.toml`).
2. Drops it into the dev-jail's `/root/.ssh/authorized_keys`
   (or the user-uid's `~/.ssh/authorized_keys` if the toolchain
   provides one).
3. Generates a fresh per-jail host keypair for sshd.
4. Updates the user's session-jail `~/.ssh/config` with:
   ```
   Host dev-myproject
       HostName 127.10.0.42
       User dev-user
       IdentityFile ~/.ssh/atrium_dev_id
       StrictHostKeyChecking accept-new
   ```

If the user wants per-jail keys (security-sensitive projects),
manifest can declare:

```toml
[ide.ssh]
key_per_jail = true     # generate a fresh keypair just for this dev-jail
```

In which case `atrium-devjail create` generates a new keypair,
stores both halves in the user's session jail at
`~/.config/atrium/devjail-keys/myproject.{pub,priv}`, and
configures SSH to use that key for this host.

## 5. Lifecycle

```
                   ┌──────────────┐
   create ───────► │  PROVISIONED │ ◄──┐
                   │              │    │ atrium-devjail rebuild
                   └──────┬───────┘    │
                          │            │
                          │ start      │
                          ▼            │
                   ┌──────────────┐    │
                   │  RUNNING     │ ───┘
                   └──────┬───────┘
                          │
                          ├─ stop ──────► STOPPED (jails released; volumes preserved)
                          │
                          ├─ suspend ───► SUSPENDED (jails frozen; resume fast)
                          │
                          └─ destroy ──► DESTROYED (jail + volumes gone)
```

### 5.1 Create

```sh
$ atrium-devjail create
```

1. Read `.atrium/devjail.toml`.
2. Validate: name not in use; manifest schema valid; declared
   network destinations on operator allow-list (per `network.md`).
3. Resolve toolchain via atrium-pkg → CAS layer hash.
4. Provision volumes via atrium-volumes (§3.4).
5. Allocate lo0 alias via atrium-jaild.
6. Generate per-jail SSH host keypair.
7. Inject user's pubkey into `authorized_keys`.
8. Launch jail via portcullisd-bootstrap.
9. Run `[setup]` script; capture output to log.
10. Update user's `~/.ssh/config` and `~/.config/atrium/devjail-list.toml`.
11. Print connect string + status.

### 5.2 Run / use

```sh
$ atrium-devjail ssh                # interactive
$ atrium-devjail run cargo test     # one-shot
$ atrium-devjail logs               # tail the jail's logs
$ atrium-devjail status             # jail state + resource usage
```

`run` SSH-execs a command and forwards stdout/stderr/exit-code.
`logs` tails the dev-jail's syslog + setup-script output.
`status` shows: state (running/stopped), resource usage
(CPU, mem, disk per volume, quota %), uptime.

### 5.3 Stop / resume

```sh
$ atrium-devjail stop               # ifconfig -alias, kill jail; volumes stay
$ atrium-devjail start              # re-launch; volumes re-mount; SSH resumes
```

Stop releases the jail process tree but preserves volumes +
SSH host keys + manifest state. Start is fast (~1 second); jail
boots, sshd starts, SSH client reconnects. Useful during reboots
or when tidying up resource usage.

### 5.4 Suspend (V2)

Future capability: freeze the jail's process state to disk
(checkpoint), restore later. Useful for "I'm context-switching
between projects; I'd like the build state cached."

V1: `stop` is the only persistence mechanism. V2: `suspend` /
`resume` for live process-state preservation. Requires CRIU-
class checkpoint/restore on FreeBSD; significant work; defer.

### 5.5 Destroy

```sh
$ atrium-devjail destroy
  → removes the jail, releases lo0 alias
  → removes volumes (with confirmation if data > 0)
  → removes user's ~/.ssh/config entry
  → removes from ~/.config/atrium/devjail-list.toml
```

Destroy is reversible only if the user reads the prompt; volumes
are gone after `--yes`. Operator policy may require a longer
soft-delete period (e.g., volumes go to a trash dir for 7 days).

### 5.6 Rebuild

```sh
$ atrium-devjail rebuild
  → re-creates the jail (new toolchain hash if manifest changed)
  → preserves volumes
  → re-runs [setup] only if manifest hash changed
```

Useful when the manifest's toolchain version is bumped: the
toolchain CAS layer changes; the dev-jail picks up the new one;
project files survive.

## 6. CLI surface

`atrium-devjail` is the single user-facing tool:

```
atrium-devjail create [--manifest .atrium/devjail.toml]
atrium-devjail destroy [--yes]
atrium-devjail rebuild
atrium-devjail start | stop | restart
atrium-devjail status
atrium-devjail ssh [-- <command...>]
atrium-devjail run <command...>          # alias for ssh -- <command>
atrium-devjail logs [--follow] [--setup]
atrium-devjail list                       # all dev-jails for this user
atrium-devjail open                       # alias for ssh; opens shell

atrium-devjail config                     # show current config
atrium-devjail config set ide.path /alt   # tweak per-jail config

atrium-devjail snapshot create <name>     # snapshot all volumes (V2)
atrium-devjail snapshot list
atrium-devjail snapshot restore <name>
```

`atrium-devjail` runs in the user's session jail; talks to:

- portcullisd-daemon (over aqueduct) for jail lifecycle ops
- atrium-volumes (over aqueduct) for volume ops
- atrium-pkg (over aqueduct) for toolchain resolution
- atrium-netd (state file) for SSH config inference

The user never invokes jaild or atrium-netd directly; the CLI
is the affordance.

### 6.1 `atrium-devjail open` and IDE integration

`atrium-devjail open` does what the user wants 90% of the time:

1. Ensures the jail is running (start if stopped).
2. Opens an interactive SSH session.
3. Lands in `[ide.default_path]` (defaults to `/atrium/source`).

For VS Code: `atrium-devjail vscode` opens the project in VS
Code via VS Code Remote, automatically:

```sh
$ atrium-devjail vscode
  → ensures jail is running
  → invokes vscode (in vscode-jail) with --remote ssh-remote+dev-myproject
  → VS Code Remote does its thing; user sees the editor on their screen
```

### 6.2 Multiple dev-jails

A user typically has 3-10 dev-jails:

```
$ atrium-devjail list
NAME            STATE      LANG       IDLE       DISK
myproject       running    rust 1.83  -          14.2G/35G
sideapp         stopped    node 20    2d 4h     2.1G/35G
experiment      running    python 3.12 4h       890M/35G
work-fork       suspended  rust 1.83  6d 12h    8.4G/35G
```

Each is independent. Stop/start/destroy is per-jail. Disk usage
per volume is reported by atrium-volumes.

## 7. Network architecture (recap)

Pulls in directly from `network.md`:

- **vscode jail** has `network.peer_jails = ["dev-*"]` and gets
  pf rules allowing it to connect to any dev-* jail's SSH port.
- **Each dev-jail** has `network.lo0_alias = true`, `network.host_alias = true`,
  `network.outbound = ...`, `network.inbound = [22]`.
- **No jail-to-jail mesh.** Dev-jails don't dial each other by
  default. If a project depends on a service jail (Postgres),
  the dev-jail's manifest declares `peer_jails = ["postgres-*"]`.
- **Default-deny everywhere.** Per `network.md`, every connection
  is blocked unless an explicit pf rule allows it.
- **/etc/hosts injection.** atrium-netd writes
  `/etc/hosts.atrium` per jail with `dev-myproject  127.10.0.42`
  entries; SSH config + name resolution both work without DNS
  service infrastructure.

The user's session jail is special — it has a broader
`peer_jails = ["dev-*", "atrium-*"]` allowing the user to SSH
into any of their jails interactively.

## 8. Storage architecture (recap)

Pulls from `storage.md` + `tessera-quotas.md`:

- **Three persistent volumes per dev-jail** (source, build,
  cache) on Tessera, each with its own quota.
- **Tessera CAS dedup** means the same toolchain layer across
  ten dev-jails costs the disk space of one.
- **Per-project size_max** is enforced by tessera-quotas; the
  manifest specifies; atrium-volumes applies; tessera enforces.
- **Snapshots** (V2) — atrium-devjail snapshot create takes a
  Tessera snapshot of all three volumes atomically.

Operator can cap per-user-or-per-project total disk in
`atrium-volumes`'s policy; per-jail quota lives in the manifest.

## 9. Comparison to Linux dev containers

What we get for free that Linux struggles with:

| Concern | Linux dev container | Atrium dev-jail |
|---|---|---|
| Editor sees project files | bind-mount + uid mapping | SSH (no mount) |
| Network policy | Docker network + iptables | atrium-netd pf anchors |
| Toolchain isolation | image layer | Tessera CAS overlay |
| Per-project storage limit | `--storage-opt size=` (some drivers) | tessera quota |
| Reproducibility | Dockerfile | `.atrium/devjail.toml` |
| Service dependencies | `docker-compose.yml` | `[[services]]` in manifest |
| Boot speed | seconds | ~1 second |
| Disk overhead | per-image (often GB) | CAS-deduped (KB-MB delta) |
| Capability boundary | namespaces + cgroups | jail + manifest cap-prompt |

What's harder than Linux:

- **First-time setup**. Linux: `docker pull`. Atrium:
  install atrium-pkg + run atrium-devjail create + accept
  capability prompts. More work, but each step is auditable.
- **Ecosystem maturity**. Docker Hub has millions of images;
  atrium-pkg is much younger. Project authors writing a
  `.atrium/devjail.toml` from scratch will find fewer
  examples.
- **Cross-platform CI**. Most CI runs on Linux; Atrium dev
  jails don't run on Linux without translation. Future:
  `atrium-devjail` could emit a Dockerfile equivalent for CI
  use.

## 10. Implementation order

| Stage | Goal | Estimate |
|---|---|---|
| 1 | `.atrium/devjail.toml` schema; parser; validator. | 3 days |
| 2 | atrium-dev-base rootfs build (atrium-pkg track; ~150MB Tessera CAS layer). | 1 week |
| 3 | Toolchain overlay templates (Rust, Node, Python, Go) for atrium-pkg. | 2 weeks (per language; can parallelize) |
| 4 | atrium-devjail CLI: create, ssh, destroy, list, status. | 1 week |
| 5 | SSH key management + ~/.ssh/config integration. | 3 days |
| 6 | `[setup]` script execution; sentinel-based first-run (per portcullis §3.4). | 2 days |
| 7 | `[[services]]` block: provisioning + cross-jail dependency wiring. | 1 week |
| 8 | VS Code Remote integration: `atrium-devjail vscode` command. | 3 days |
| 9 | Snapshot / restore (V2). | 1 week |

Total V1 (stages 1-8): ~4-5 weeks focused. atrium-pkg
toolchain overlays (stage 3) is the biggest variable; one
language at a time, ship Rust first since that's our primary
internal use.

## 11. Open questions

1. **Multi-dev-jail shared cache.** Two Rust projects on the
   same toolchain version could share a single `~/.cargo`
   cache volume. Saves disk + warms cargo's metadata cache
   for new projects. Adds complexity (concurrent writers, lock
   files); defer to V2.
2. **Editor-state persistence across destroy/recreate.** VS
   Code's per-workspace settings (open files, cursor positions)
   live inside the dev-jail. Destroying loses them. V2 may
   carve VS Code's `~/.vscode-server` into a separate volume
   that's preserved across rebuild.
3. **Devcontainer compatibility.** Could atrium-devjail consume
   `.devcontainer/devcontainer.json`? Many projects already
   ship one. Translation layer (read devcontainer.json, emit
   equivalent atrium config) is a small, useful tool. V2.
4. **Dev-jail GUI**. Forum (D3) could surface a dev-jail panel:
   list, create, connect, snapshot. Right now CLI-only;
   GUI is post-D3.
5. **Pre-create from a template.** `atrium-devjail create
   --template rust-cli` writes a starter `.atrium/devjail.toml`
   tuned for the common cases. Small UX improvement; defer.
6. **Cross-host dev-jails (D2.5+).** When Atrium federates
   across multiple hosts, "create dev-jail on the fast box,
   SSH from my laptop" is interesting. Out of V1 scope;
   future spec.
7. **GPU access for dev-jails.** ML/CUDA workflows need the
   dev-jail to see the GPU. Manifest gains
   `[capabilities] graphics_compute = true` (different from
   graphics-for-rendering); maps to atrium-gpu device cap.
   Pending the GPU stack landing per `gpu-abi.md`.

## 12. Worked example: the Atrium repo's own .atrium/devjail.toml

The Atrium repo could ship its own `.atrium/devjail.toml`:

```toml
[devjail]
name = "atrium"
description = "Atrium platform development"

[toolchain]
languages = ["rust", "shell", "cpp"]
rust_version = "1.83"

[packages]
freebsd = ["pkgconf", "openssl", "rust-cbindgen", "binutils",
           "qemu-system-aarch64", "git", "tmux"]
cargo   = ["cargo-watch", "cargo-audit", "cargo-machete"]

[volumes]
source.size_max = "5G"
build.size_max  = "20G"        # the rust target/ gets large
cache.size_max  = "10G"        # ~/.cargo + Tessera build artifacts

[network]
outbound = "any"               # we fetch from many places during dev

[setup]
script = """
# Atrium dev-bootstrap.
( cd portcullis    && cargo fetch )
( cd atrium-tessera && cargo fetch )
( cd aqueduct       && cargo build --bins )
"""

[[services]]
name  = "atrium-vm"            # the QEMU VM we develop against
image = "atrium-qemu-aarch64-vm:latest"
volumes = ["disk:50G"]
ports   = [2222, 4444, 5900]   # SSH, serial, VNC

[ide.vscode]
extensions = ["rust-lang.rust-analyzer", "vadimcn.vscode-lldb"]
```

Anyone cloning the Atrium repo and running `atrium-devjail
create` gets the same dev environment — no "install these 10
things first" instructions in CONTRIBUTING.md.

## 13. References

- [`portcullis.md`](portcullis.md) — capability manifest
  schema; `[setup]` block; capability prompts.
- [`storage.md`](storage.md), [`atrium-volumes.md`](atrium-volumes.md),
  [`tessera-quotas.md`](tessera-quotas.md) — per-project
  volume provisioning + size limits.
- [`network.md`](network.md), [`atrium-netd.md`](atrium-netd.md) —
  inter-jail SSH networking.
- future `atrium-pkg.md` — toolchain overlay distribution.
- VS Code Remote SSH documentation (microsoft) — the IDE
  integration we target.
- Linux dev containers spec (`devcontainer.json`) — for the
  interoperability question (§11.3).
