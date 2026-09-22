# Portcullis — jail launcher + capability manifest

Status: design + partial implementation (D2.5).
Last updated: 2026-05-08.

## Implementation status

The privsep architecture (jaild + portcullisd, §0.5) is **alive end-to-end** as of 2026-05-08. Production-shape boot:

```
service atrium-jaild                       # privileged jail broker
service atrium-volumes                     # volume allocation (spec/atrium-volumes.md)
service atrium-portcullisd-daemon          # capability mediator (aqueduct, CLASS_PORTCULLIS=6)
service atrium-portcullisd-bootstrap       # supervisor; reads /etc/atrium/services.d/
```

Each ships an `rc.d` script in-tree (`portcullis/{jaild,atrium-volumes,portcullisd}/etc/`).

**Live properties** (VM-verified):
- Manifest schema: `enabled`, `name`, `path`, `[[mounts]]`, `[[volumes]]` (with `[volumes.init]` first-run sentinels), `[exec]`, `[supervision]`, `[capabilities]`.
- `[capabilities] attach_mount = true` derives a ro_nullfs mount of `/var/run/atrium/caps/portcullisd/` → `/atrium/sockets/portcullisd/` at jail-create time. In-jail services connect to the daemon over aqueduct.
- Daemon-side authz: `getpeereid(2)` on each connection → cross-checked against `manifest.exec.uid` so a uid-1001 caller in jail-A cannot forge `jail_name = jail-B` to ride B's broader allow-list.
- jaild grew `AttachMount` / `DetachMount` (spec/storage.md §6.2) + `runtime_mounts` state + orphan reconcile on jaild restart.
- Failure-budget supervisor (cost-proportional retries, tombstone-style retire keeps kqueue udata stable).
- Graceful shutdown: SIGTERM/SIGINT → close procdescs, unmount per-jail mounts, RemoveJail (which also drops the jail's runtime mounts).

**Deliberately deferred** (V1):
- ~~Per-jail rootfs trees.~~ Done 2026-09-13: every shipped manifest runs on a real root under `/var/lib/atrium/jails/`, and jaild refuses `path = "/"` (see the §9.1 note).
- Capability prompt UI (Forum integration — D3 dependency).
- Multi-cap composition smoke (single `attach_mount` capability is exercised; multi-cap layout is designed but no second cap exists yet).
- `portcullis launch <app-id>` user-app CLI (the daemon's underlying primitives are all there; the user-facing wrapper is a small additional slice).

See [`ROADMAP.md`](../ROADMAP.md) §D2.5 for per-phase status.

The piece of Atrium that turns "an app" into "a running, isolated,
capability-scoped process." Portcullis reads each app's
`atrium.toml`, builds a jail with exactly the capabilities the
manifest declares, and execs the app inside. Forum (D3, the dock /
launcher) calls Portcullis; Vestibulum (D2, login) launches the
session supervisor that does likewise.

Portcullis is to Atrium what `systemd-nspawn` is to Linux + the
permissions-model of mobile OSes — but built natively on FreeBSD
jails, devfs.rules, nullfs, rctl, and the Atrium substrate
(Tessera CAS-FS, aqueduct).

## 0.5 Architecture (privsep — post-V7 revision)

Originally this spec described a single `portcullisd` that did
both policy interpretation and `jail_set(2)`. Validation work in
2026-05-07 (`scratch/jail-smoke/`) showed two relevant facts:

1. FreeBSD's hierarchical jails work cleanly: a jail with
   `children.max>0` can host a process that creates child jails
   via `jail_set`. Confirmed on 16.0-CURRENT.
2. `jail_set` is **disabled** under Capsicum (`cap_enter()` →
   `ECAPMODE`). Combining Capsicum confinement of the policy daemon
   with dynamic jail creation requires privilege separation.

Architecture revised accordingly. Final shape:

```
host (kernel + init)
└── jaild-jail          PRIVILEGED BROKER (~500 LoC, audited)
    │                   - sole caller of jail_set / jail_remove /
    │                     pdfork / execve
    │                   - validates each request from portcullisd
    │                     against /etc/atrium/jaild.policy.toml
    │                   - cannot itself be Capsicum'd
    │
    ├── portcullisd-jail POLICY DAEMON (Capsicum'd after init)
    │                   - parses atrium.toml manifests
    │                   - capability → jail-spec translation
    │                   - sends specs to jaild over a pre-opened
    │                     socket fd; never touches global namespace
    │
    ├── frescod                     ┐
    ├── atrium-devevents            │ system services, child jails
    ├── vestibulum  (per seat)      │ of jaild-jail; managed by
    └── user-N-supervisor           │ portcullisd via jaild
        └── apps (grandchildren)    ┘
```

Pattern: OpenSSH/qmail-style privsep. The privileged TCB is jaild
(~500 LoC, no business logic, just request validation +
`jail_set`/`execve`). The complex interpreter (portcullisd) is
Capsicum-confined; even with a portcullisd RCE, an attacker only
gets to ask jaild to do things, and jaild refuses anything outside
the static policy file's allow-list.

**Validating tests** (committed as ground truth):

- `scratch/jail-smoke/jail-smoke.c` — confirms hierarchical jails
  + `ECAPMODE` semantics on 16.0-CURRENT.
- `scratch/jail-smoke/jaild-privsep.c` — confirms the full
  privsep round-trip works: portcullisd-in-cap-mode can read/write
  a pre-opened socket, cannot `open(/etc/passwd)`, cannot
  `jail_set`, but can ask jaild over the socket and get a jid back;
  jaild's allow-list rejects disallowed names.

**Companion specs:**

- `docs/spec/jaild-policy.md` — the jaild allow-list schema and
  per-field validation rules. Sample at `etc/jaild.policy.toml`.
  Rust types and parser at `portcullis/jaild-policy/`.
- `docs/spec/login-handoff.md` — boot-to-session protocol on the
  privsep arch. `pdfork(2)` + `EVFILT_PROCDESC` is the spine; works
  in Capsicum mode.
- `docs/spec/gpu-isolation.md` — invariants the kernel GPU driver
  must enforce before Portcullis is allowed to grant the
  `gpu-direct` capability. Default `render-only` capability
  (frescod-mediated, no device access) bypasses this question
  for most apps.
- `docs/spec/service-management.md` — decomposition principle vs
  systemd: which gaps fold into portcullisd vs which go into a
  dedicated daemon (`atrium-log`, `atrium-timer`, …). Binding
  decision rule for future "should this go in portcullisd?"
  questions; the default answer is *separate daemon*.
- `docs/spec/storage.md` — per-jail volume allocation via
  `atrium-volumes` (new), Tessera as default backend with ZFS /
  plain alternatives, named backend instances per operator,
  static + dynamic mount lifetime via jaild's mount-broker
  protocol.
- `docs/spec/network.md` — jail networking: capability classes
  (`disable` / `lo0_alias` / `vnet` / `host_alias`), jaild
  protocol extension, `atrium-net` GUI mediator daemon, lo0
  alias allocation, inter-jail policy.
- `docs/spec/atrium-volumes.md` — the volume-allocation broker:
  wire protocol, plugin model (tessera/zfs/plain/tmpfs), state
  file, lifecycle.
- `docs/spec/atrium-pkg.md` — Atrium package format and install
  path: CAS-bundle ingest, manifest drop, atomic update, local +
  remote (V1) registries.

The remainder of this document is largely unchanged in shape; "the
daemon" is now logically split between jaild (privileged broker)
and portcullisd (policy daemon), but the concepts (manifest schema,
capability classes, jail layout, prompt UX, lifecycle) are the
same. Where the privsep distinction matters, it's called out
explicitly.

## 0. Naming + role

`portcullis` (CLI + library), `portcullisd` (long-running
supervisor for capability prompts and jail lifecycle).

Naming: a portcullis is a defensive iron grating that closes off
a castle gate — apt for "the thing that decides what gets in and
what doesn't."

## 1. Goals

- **One declarative source of truth per app.** `atrium.toml`
  lists capabilities; everything Portcullis does is mechanically
  derived from it.
- **Default-deny.** No capability not listed is granted. Apps see
  exactly what they asked for and nothing else.
- **Kernel-enforced boundaries.** Capabilities translate to
  filesystem mounts, devfs rules, jail flags, rctl limits — all
  enforced by the FreeBSD kernel, not by a userspace policy
  daemon.
- **Composable with everything we built.** Tessera CAS-FS for
  jail trees with cross-jail dedup; aqueduct capability
  sockets nullfs-mounted per the manifest; binsplit-deduped
  function blobs (D1.7) shared across all jails on the host.
- **Fast cold-launch.** Jail creation + exec ≤ 100 ms for typical
  apps. (FreeBSD jails are kernel objects, not VMs — this is
  achievable.)
- **First-launch capability prompt.** New capabilities the user
  hasn't approved trigger a confirmation UI before the jail
  runs. Subsequent launches are silent unless the manifest
  changes.

## 2. Non-goals

- **Not a package manager.** Apps arrive as already-installed
  Tessera trees (via `tessera-import` or higher-level tooling).
  Portcullis launches them; it doesn't fetch, verify signatures
  beyond manifest checksums, or manage upgrades. (A future
  `pkg`-equivalent that wraps Portcullis is downstream of D2.5.)
- **Not the dock / launcher UI.** Forum (D3) is what the user
  clicks; Forum invokes Portcullis. Portcullis exposes a CLI +
  IPC surface, no graphical UI of its own (except the capability
  prompt, which is system-modal).
- **Not a VM.** Atrium apps are FreeBSD processes, not micro-VMs.
  Bhyve / firecracker-style isolation is out of scope —
  jails + capability scoping is the trust boundary.
- **Not a network policy framework.** Network capability is
  coarse (none / loopback-only / full). pf-style fine-grained
  rules are out of scope; an app that needs them runs as a
  privileged service, not a desktop app.
- **Not multi-user concurrent.** Portcullis runs per-user-session.
  Cross-user isolation is the OS's job, not Portcullis's.

## 3. atrium.toml schema

Lives at the root of every Atrium app's tree. Validated at
install time (`tessera-import` enforces structural validity)
and re-checked at every launch (Portcullis re-parses).

### 3.1 Concrete example

```toml
# atrium.toml — atrium-edit (a hypothetical text editor)
[app]
id          = "org.atrium.edit"
name        = "Atrium Edit"
version     = "1.2.3"
entry       = "bin/atrium-edit"   # path within the app tree
description = "Lightweight text editor for the Atrium platform"

[capabilities]
graphics    = "fresco"            # display: standard Fresco socket
clipboard   = true                # IPC: clipboard service
notify      = true                # IPC: notification service
open-uri    = true                # IPC: broker (so the editor can ask
                                  #     the system to open URLs)
filesystem  = ["~/Documents", "~/Projects"]
                                  # nullfs-mount these into the jail
                                  # (read-write by default)
network     = "none"              # none | loopback | full
audio       = false               # no audio access

[capabilities.fonts]              # read-only system-font access
mode        = "read-only"
paths       = ["/usr/share/fonts"]

[setup]                           # optional — runs once on first launch
command     = "scripts/firstrun.sh"
timeout     = "120s"

[setup.capabilities]              # only-during-setup overrides;
network     = "full"              # any capability can be overridden in
                                  # either direction; reverts to the
                                  # runtime [capabilities] value after
                                  # setup completes

[resources]                       # rctl-enforced limits, optional
memory      = "512M"
cpu         = 200                 # percent of one core (200 = 2 cores)
files       = 1024                # ulimit -n equivalent

[supervision]
restart     = "on-crash"          # never | on-crash | always
keep-alive  = false               # if true, restart immediately on exit
```

### 3.2 Capability classes

| Capability | Grants | Mechanism |
|---|---|---|
| `graphics = "fresco"` | Talk to compositor | nullfs `/atrium/sockets/fresco.sock` + devfs `/dev/fresco0` |
| `gpu.access = true` | Open `/dev/atrium-gpu0`, allocate ordinary BOs, submit GPU work (aqueduct-gpu). Required by Vulkan games via atrium-vk-icd, by any app that drives the GPU directly. Does **not** grant scanout. | devfs `/dev/atrium-gpu0` + portcullisd cap-mediator check at aqueduct-gpu handshake. Kmod enforces fd-scoped resource isolation. See `aqueduct-gpu.md` §12.3. |
| `gpu.scanout = true` | Additionally allocate scanout BOs (`ATRIUM_GPU_BO_SCANOUT`) and call `page_flip`. Granted only to display-server processes (`frescod` today). **Restricted capability**: not user-grantable without explicit policy approval. | Kmod cross-checks the calling fd's cap token (set via `IOC_SET_CAPS` at portcullisd-mediated open time) against the `ATRIUM_GPU_BO_SCANOUT` flag at `IOC_ALLOC`; rejects with `EPERM` if not granted. See `aqueduct-gpu.md` §12.4. |
| `clipboard = true` | Talk to clipboard daemon | nullfs `/atrium/sockets/clipboard.sock` |
| `notify = true` | Send notifications | nullfs `/atrium/sockets/notify.sock` |
| `open-uri = true` | Ask broker to open URLs | nullfs `/atrium/sockets/broker.sock` |
| `audio = true` | Capture/play audio | nullfs audio.sock + devfs `/dev/dsp*` |
| `filesystem = [...]` | Read/write listed paths | nullfs each path |
| `fonts.mode = "read-only", paths = [...]` | Read fonts | nullfs read-only |
| `network = "none"` | (default) | jail flag `allow.raw_sockets=0`, no interface |
| `network = "loopback"` | Bind/connect on 127.0.0.1 | shared loopback alias |
| `network = "full"` | Real network | shared default interface |
| `tessera-cas-read = true` | Read global CAS | nullfs read-only of `/var/lib/tessera/cas` |
| `usb-hid = true` | Read input devices | devfs `/dev/input/eventN` |
| `camera = true` | Read camera device | devfs `/dev/video0` |
| `microphone = true` | Read mic input | devfs audio capture |

Special / restricted (`tessera-cas-read`, `usb-hid` for non-input
apps, `gpu.scanout`) require explicit policy approval beyond user
prompt — only granted to system services or apps with manual admin
override. In particular, `gpu.scanout` is restricted to the
display-server process (`frescod`); without this restriction a
malicious app could allocate scanout BOs and observe or interfere
with what the user sees on screen. See `aqueduct-gpu.md` §12.4 for
the kmod-level enforcement mechanics.

### 3.3 Validation rules

At parse time:
- `app.id` matches `^[a-z][a-z0-9.-]*$` (reverse-DNS style).
- `app.entry` is a relative path within the tree, points at an
  executable file.
- `capabilities.filesystem` paths are absolute (after `~/`
  expansion to the user's home) and not under `/atrium/`,
  `/dev/`, `/var/lib/tessera/` (those are managed mounts).
- `network` ∈ `{"none", "loopback", "full"}`.
- Reserved keys (`tessera-cas-read`, `camera`, `microphone`,
  `usb-hid`-as-non-graphics) require either user-prompt-approval
  OR a `policy.toml` admin grant.
- Unknown keys are warnings, not errors (forward compatibility).

## 3.4 Setup phase (first-run script)

Portcullis intentionally does **not** know what an app installs.
Apps that need to `pkg install` dependencies, download model
weights, generate keys, populate caches, or do any other
imperative bootstrap own that work in their `[setup]` section.

```toml
[setup]
command = "scripts/firstrun.sh"   # path within app tree
timeout = "120s"                  # fail if it hangs

[setup.capabilities]              # only-during-setup overrides
network = "full"                  # uses the same capability vocabulary
                                  # as [capabilities]; bidirectional
                                  # (setup may have MORE or LESS than
                                  # runtime); reverts to the runtime
                                  # value after setup completes
```

### Mechanics

On launch, Portcullis checks the per-app overlay for a sentinel
file `.atrium-firstrun-done`:

- **Sentinel present** (subsequent launches): skip setup, jump
  to runtime.
- **Sentinel absent + `[setup]` exists** (first launch, or after
  reinstall): apply runtime capabilities + setup additions
  (network, etc.); execute `setup.command` via jail.conf's
  `exec.created`. On clean exit, write sentinel. On failure,
  leave sentinel absent so next launch retries.
- **Sentinel absent + no `[setup]`**: write sentinel and proceed.

The script runs inside the same jail the app will run in,
with its working directory at the app's tree root. Whatever it
puts in `/usr/local/...`, `/etc/...`, `/var/...` lands in the
app's overlay (Tessera-backed; cross-jail dedup'd by content).

### Two-phase capabilities

Capability resolution is straightforward:
- During the setup phase: merge `[setup.capabilities]` over
  `[capabilities]` (overrides win, in either direction); the
  resulting set is what the jail sees.
- After setup completes: revert to plain `[capabilities]`.

The override is bidirectional. Common pattern: setup needs
network for `pkg install`, runtime doesn't. Less common but
valid: setup is purely local config-file generation and
deliberately drops the network the runtime app uses, as a
defense-in-depth measure against compromised setup scripts.

There's no separate vocabulary for setup-only flags — the same
capability keys mean the same thing in both contexts. A user
prompt at first install shows the runtime set + a diff for
setup, regardless of override direction:

```
"Atrium Edit" wants:

  Runtime (every launch):
      ✓ Display (Fresco)
      ✓ Clipboard
      ✓ Notifications
      ✓ Read/write your Documents folder
      ✗ No network

  During one-time first-run setup, these change:
      network: none → full     (additional access)

[Allow once]   [Allow always]   [Deny]
```

(If a setup phase REDUCED something, the same diff format
shows it — `filesystem: ["~/Documents"] → []` for example.)

After setup exits cleanly, Portcullis tears down the setup-
phase jail and re-creates the jail with runtime-only caps for
the first real launch (or uses jail.conf's exec.created vs
exec.start phasing if the override can be expressed in one
config block).

### What apps typically do in setup

- `pkg install openssl libxml2` — pull FreeBSD pkg deps. Tessera
  CAS-FS dedups identical files across all jails that did the
  same install (one disk copy of openssl regardless of how many
  apps).
- `cargo install` or other language-specific package fetches.
- Download large assets (ML models, asset bundles) from the web.
- Run database migrations, generate keys, prepare caches.
- Anything else — Portcullis doesn't care.

### Cross-jail dedup is automatic, not orchestrated

Because all app overlays are subtrees of the single shared
Tessera volume (see §4.1), files written by one app's setup are
content-addressed by Tessera. If app B's setup writes the same
file (e.g., the same `libssl.so.3`), the bytes converge to one
stored copy. Note the convergence is *at rest, after the next
repack pass*, not at write time — overlays run the `deferred`
dedup policy (§4.1, tessera-fs.md §20.2) so that an app cannot
probe the system by watching whether its own writes deduplicate.
The steady-state disk math is unchanged; only the moment of
physical convergence moves.

Portcullis doesn't need to know about pkg, the pool concept, or
any specific package manager. The dedup is a property of the
storage layer, not the launcher.

### Sharing a fetch cache (optional)

Apps that use `pkg install` benefit from a shared download cache
to avoid redundant downloads. The convention is:

```
/var/cache/atrium/pkg/  ← shared across jails (read-only nullfs)
```

Apps' setup scripts can opt in by setting `PKG_CACHEDIR` to that
path. This is a hint, not a requirement; Portcullis exposes the
mount point via the `[capabilities]` system if the app declares
it (`fetch-cache = "pkg"`).

### Re-running setup (upgrades)

If the app's manifest changes (manifest hash mismatch on grant),
the user is re-prompted; if they re-grant setup, Portcullis
clears the sentinel so setup re-runs on next launch. Apps should
write idempotent setup scripts.

`portcullis reinstall <app>` wipes the overlay (or just the
sentinel) on user request.

### Mechanics

When `tessera-import` materialises an app tree, it reads
`[packages.freebsd]`. If the app's overlay doesn't already have
the declared packages installed (lookup via pkg's own
`/var/db/pkg/local.sqlite` inside the overlay), it runs:

```
jail -c name=atrium-pkginstall-<id>           \
       path=/var/lib/atrium/jails/<id>/...    \
       ip4.addr=... vnet=...                  \  # transient network
       command=/usr/sbin/pkg install -y openssl-3.0
```

pkg downloads, verifies signatures, runs install scripts, writes
files into the jail's rootfs+overlay. All standard FreeBSD pkg
behaviour. Network access is transient — granted only for the
install operation, dropped before the app launches normally.

### Shared fetch cache

A host-side cache at `/var/cache/atrium/pkg/` mounted (read-write
to the install jail, read-only to the app jail's pkg if the app
ever runs `pkg upgrade`) avoids redundant downloads. Standard
pkg.conf knob:

```
PKG_CACHEDIR: /var/cache/atrium/pkg
```

App A's install fetches openssl-3.0.13 into the cache; App B's
install pulls from cache, no network round-trip. Cache is
content-addressed by package signature — no ambiguity about
"which 3.0.13 is this."

### Storage outcome

Each app's jail has its own pkg database, its own install-script
side effects, its own etc/* contributions. The actual binary and
data files (`/usr/local/lib/libssl.so.3`, `/usr/local/share/...`)
are byte-identical across jails depending on the same package
version. Tessera CAS-FS hashes those files and stores **one
physical copy** regardless of how many jails reference them.

What's NOT deduped (small, per-jail):
- `/var/db/pkg/local.sqlite` — pkg's local database; per-jail
  state (timestamps, install order). A few MB per jail.
- Install-script effects on `/etc/passwd`, `/etc/group`, etc. if
  any (rare for FreeBSD packages).

What IS deduped (large, the actual content):
- `/usr/local/lib/*` — shared libraries
- `/usr/local/share/*` — data, locales, docs
- `/usr/local/include/*` — headers
- `/usr/local/bin/*` — pkg-installed binaries
- All of these are byte-identical across same-version pkg installs

For 50 apps depending on openssl, the storage cost is:
`(one openssl install's content)` + `(50 × pkg-DB-overhead ~few MB)`
vs. the naive 50 × full-install-size.

### Multi-version

Each app's jail has whatever version it requested. App A pinning
openssl-3.0 and App B pinning openssl-3.1 just means each jail
ran a different `pkg install`. Both versions exist on disk (one
copy each, via Tessera CAS), no conflict.

### Updates

App author bumps the manifest version constraint and re-publishes;
`tessera-import` re-runs the install in the jail with the new
constraint. Standard atrium-package-upgrade story; doesn't need
special pool-update tooling.

For ad-hoc security updates (CVE drops on openssl): users can
either wait for app authors to bump constraints, or trigger a
"refresh installed packages" sweep via `portcullis pkg refresh`
that re-runs `pkg upgrade` inside each app's jail (subject to
the constraint in the manifest). All standard.

### Trust model

- Packages are pkg-signed; signature verification happens
  per-install (standard pkg behaviour).
- Install runs in the app's own jail with a transient network
  capability — install-time access to pkg repos doesn't expand
  the app's runtime capabilities.
- A compromised package compromises the apps that installed it,
  same as on any FreeBSD system. Tessera CAS doesn't add or
  remove this risk.

### Transitive dependencies

`pkg install openssl` auto-installs its closure (libfoo, libbar,
etc.) via pkg's own dependency resolution. The app gets
everything it needs — no special handling required from
Portcullis. The recorded "what's installed" lives in the jail's
pkg DB; users can `portcullis describe <app>` to see it.

### Why per-jail install over a host-side pool

The original design (early 2026-05-03 draft) proposed a host-side
package pool with nullfs mounts into jails. Trade-off analysis:

| | Pool | Per-jail install + shared cache |
|---|---|---|
| `pkg install` runs | 1 per (pkg, ver) | N per N jails |
| Storage on disk | 1 copy via mounts | 1 copy via Tessera CAS |
| Network fetches | 1 per (pkg, ver) | 1 per (pkg, ver) (cache) |
| Architectural complexity | high | low |
| App's view | bespoke mount layout | normal pkg layout |
| `pkg info` inside jail | doesn't work | works |
| Multi-version | per-(pkg, ver) dirs | per-jail DBs |
| Conflict resolution | needed (pool overlap) | none — each jail isolated |

Storage outcome is identical (Tessera CAS handles dedup either
way). Install-time cost differs (N × seconds vs. 1 × seconds) but
amortizes to nothing for steady-state usage. Complexity cost of
the pool design is real and ongoing.

**Per-jail install + shared fetch cache wins** on simplicity for
identical storage outcome.

## 3.5 Using FreeBSD rc(8) inside the jail

The jail is a normal FreeBSD environment. `/etc/rc`, `service(8)`,
and the `rc.d` framework are all available. **rc.d is for
background helpers and one-shot setup, NOT for the foreground
app.** Apps stay apps; rc.d holds the supporting machinery.

Two specific use cases for rc.d in an app's tree:

1. **First-run setup** — rc.d scripts with `KEYWORD: firstboot`
   that run once via `/etc/rc firstboot`.
2. **Background helpers** — long-running daemons the app needs
   (a sync worker, an indexer, a local IPC bridge) declared as
   normal rc.d services, listed in `[app].helpers`, started
   before `entry` and stopped after.

The foreground app itself is always launched directly via
`[app].entry` — never via `service` or rc.

### Pattern A — single foreground binary

```toml
[app]
entry = "bin/atrium-edit"
```

Portcullis execs the binary directly. No rc, no helpers.
Most apps fit here.

### Pattern B — foreground app with background helpers

App ships an indexer daemon as an rc.d script + rc.conf enable
flag, exactly like a normal FreeBSD service:

```
usr/local/etc/rc.d/atrium-edit-indexer    ← rc.d script
etc/rc.conf.d/atrium-edit-indexer         ← contains: atrium_edit_indexer_enable="YES"
bin/atrium-edit                            ← foreground app
```

atrium.toml is unchanged from Pattern A:

```toml
[app]
entry = "bin/atrium-edit"
```

The helpers come up automatically because every jail launch
runs `/etc/rc` (which iterates rc.d, starting everything
enabled in rc.conf), then execs `[app].entry`. Standard FreeBSD
jail behavior — Portcullis doesn't need a special schema field
for "helpers" because the rc.conf model already expresses
exactly that.

Portcullis lifecycle (per launch):

1. (Setup phase if first launch — see §3.4.)
2. `/etc/rc` (jail.conf `exec.start`) — brings up enabled services.
3. Exec `[app].entry` as the foreground process.
4. When entry exits: `/etc/rc.shutdown` (jail.conf `exec.stop`)
   — stops services in reverse order.
5. Tear down jail (`jail -r`).

The foreground app is NOT in rc.d. It's the user-facing process,
exec'd by Portcullis directly. Helpers are background and managed
via rc/rc.conf; they live and die with the jail. No new Atrium
schema for any of this — it's stock FreeBSD.

### Pattern C — first-run setup via rc firstboot

FreeBSD's `firstboot` mechanism marks rc.d scripts that should
run only on a system's first boot. Atrium leverages this for
[setup] phase:

```toml
[setup]
command = "/etc/rc firstboot"
timeout = "300s"

[setup.capabilities]
network = "full"
```

App ships a firstboot rc.d script:

```sh
# usr/local/etc/rc.d/atrium-edit-setup
# PROVIDE: atrium-edit-setup
# REQUIRE: NETWORKING firstboot
# KEYWORD: firstboot

. /etc/rc.subr
name="atrium_edit_setup"
start_cmd="atrium_edit_setup_start"
atrium_edit_setup_start() {
    pkg install -y openssl libxml2
    /usr/local/bin/atrium-edit-init-config
}
load_rc_config $name
run_rc_command "$1"
```

`/etc/rc firstboot` runs all such KEYWORD-tagged scripts.
FreeBSD touches `/var/db/firstboot` when done; Portcullis writes
its own sentinel `.atrium-firstrun-done` on top so subsequent
launches skip the setup phase entirely.

### Why this matters

- **Familiar.** FreeBSD admins already write rc.d scripts for
  helpers and setup. Atrium app authors reuse what they know.
- **Composable.** Service dependencies (`# REQUIRE: foo bar`),
  ordering (`# BEFORE: baz`), lifecycle (`service foo
  status/start/stop/restart`) all work.
- **Logs.** rc.d output goes to standard FreeBSD logging
  locations; no Atrium-specific log plumbing.
- **No new framework.** Portcullis just runs commands. The
  orchestration richness for helpers + setup comes from rc,
  which is already in the jail.
- **Foreground stays foreground.** The user-facing app isn't
  buried in a service script — it's at the top level of
  `[app].entry`, easy to see and reason about.

### Caveats

- Pattern A is often best — don't add helpers if you don't
  need them. Empty rc.conf means `/etc/rc` is essentially a
  no-op.
- rc.d scripts that try to modify the host (loading kmods,
  writing outside the jail) won't work, by design.
- Helpers live and die with the jail. If a helper should
  survive the foreground app (rare for desktop apps), it's
  a system service, not a per-app helper — runs in its own
  jail and is reachable via aqueduct.

## 4. Jail filesystem layout

### 4.0 Apps directory

Installed apps live at `/var/lib/atrium/apps/<app.id>/`. This is
the convention `tessera-import` writes to and `portcullis launch
<app-id>` resolves against.

```
$ tessera-import some-source-tree /var/lib/atrium/apps/org.atrium.edit
$ portcullis launch org.atrium.edit
```

The directory tree itself lives on the shared Tessera volume
(see §4.1 below), so cross-app file dedup is automatic.

`portcullis launch <arg>` heuristic: if `<arg>` contains `/`,
starts with `.`, or doesn't match `^[a-z][a-z0-9.-]*$`, it's
treated as a filesystem path; otherwise it's resolved against
`/var/lib/atrium/apps/`. This lets development workflows pass a
local tree path while production launches use ids.

### 4.1 Single shared Tessera volume

**All Atrium jails are subtrees of one underlying Tessera volume,
not separate per-jail volumes.** This is the load-bearing
architectural choice that makes cross-jail dedup work:

- Tessera's CAS layer (pack registry, blob hashes, dedup) is
  per-volume.
- Two subtrees of the same volume → blobs are shared via the same
  pack registry → cross-jail dedup is automatic.
- Two separate volumes → blobs are independently stored → zero
  cross-volume dedup.

The shared volume lives at `/var/lib/atrium/store.tessera`
(or wherever the host installer puts it). All jails' rootfs +
overlay + (per-app pkg-installed files) are subtrees inside
this single volume.

Dedup is automatic but **not uniform** — it is per-dedup-domain
policy (tessera-fs.md §20), because observable cross-jail dedup
is an existence oracle (a jail could otherwise probe what files
exist elsewhere on the system by writing candidates and watching
free space / write timing). Portcullis maps the policy onto the
three trees of §4.2:

| Tree | Writer | Dedup policy |
|---|---|---|
| `apps/<id>/` | atrium-pkg / tessera-import (trusted, rank ≥3) | `global` — synchronous, total. This is where the N-apps-≈-1× disk thesis is won. |
| `overlays/<id>/` | the jailed app (untrusted) | `deferred` (default) — content-independent write behavior; physical dedup converges at repack. `salted` iff the manifest sets `privacy = true` on the volume. |
| `jails/<id>/` | mountpoint only | n/a (no persistent content) |

> **Adopted 2026-09-12 — overlays become per-app VOLUMES, and their policy
> reverts to `global`.** The row above says `deferred` because, in a layout
> where every overlay is a directory on one shared volume, `deferred` is the
> only thing closing §20.1 channel 1 (see the correction below: the
> per-directory quota does *not* scope `statfs`). Making each overlay its own
> Tessera volume mounted with a whole-FS quota replaces that behavioural
> mitigation with a structural one:
>
> ```
> mount -t tessera -o tessera.quota_bytes=N /dev/<overlay-vol> \
>       /var/lib/atrium/overlays/<app.id>
> ```
>
> Measured on the reference kmod:
>
> | claim | result |
> |---|---|
> | whole-FS quota scopes `statfs` (§3.6) | 256 MiB visible vs a 4096 MiB pool |
> | physical dedup still active inside the volume | `publish_dedup_chunked` +1 on a duplicate |
> | the jail observes **logical** usage | 248 MiB free of a 256 MiB quota, pool holding 261 MiB |
>
> The third row is why `global` is safe here. `f_bavail = (limit − used_bytes)
> / bsize` and `used_bytes` is **logical** (tessera-quotas.md §3.2), so a
> duplicate consumes full quota whether or not it deduped physically. The
> number a jail can observe is therefore **content-independent by
> construction** — the oracle is closed by the accounting, not by how the write
> path behaves — and the volume contains only that app's own content, so there
> is nothing of anyone else's to probe for. `deferred`'s transient double
> storage is no longer needed.
>
> **What this does NOT change:** `apps/` stays a single shared volume on
> `global`. tessera-fs.md §20.2 is explicit that the N-apps-≈-1× thesis is won
> *entirely* on trusted-ingest content, so keeping one apps volume preserves
> the whole disk-cost win. A volume is the dedup boundary, so per-app overlay
> volumes do lose cross-*overlay* dedup — which §20.2 already judges marginal,
> since it is user data that rarely repeats between jails. Dedup *within* an
> overlay is unaffected.
>
> **Cost, stated honestly.** A Tessera volume carries ~6.4% fixed overhead
> (measured: 257 MiB on 4 GiB, 33 MiB on 512 MiB — it scales, so it cannot be
> amortised away), and each overlay volume is a separate mount with its own
> flush gate, dirty lists, GC context and pinscan. Fifty apps with 1 GiB
> overlays is ~3.2 GiB of overhead and fifty background GC loops. Provisioning
> also becomes volume-create rather than `mkdir`. Re-measure at the app count
> actually expected before committing to it at scale.
>
> ⚠ Do **not** take the middle option of one shared *overlays* volume separate
> from `apps/`. It hides app content from the oracle but still leaks between
> jails, so it pays the split without buying the structural guarantee.

> **Implemented 2026-09-12** in `portcullis-overlay`, called from launch step 0
> and from `portcullis remove`. Three things the design above did not say:
>
> **Where the volume comes from.** `<overlay-vol>` is an image file under
> `/var/lib/atrium/overlay-vols/<app.id>.img` attached through `md(4)`:
> `mkfs-tessera --create -s <MiB>`, `mdconfig -a -t vnode -f`, then the mount.
> Every attach checks `mdconfig -lv` first — two `md` devices over one image is
> corruption waiting to happen.
>
> **Overlays are 64 MiB to 8 GiB, default 1 GiB.** `[resources] storage` in the
> manifest sets the size and is clamped, loudly, never silently.
>
> *History.* The first implementation was capped at 384 MiB by the kmod, not by
> policy: image creation ends in `ftruncate`, and a truncate-extend on Tessera
> built the whole new file in one contiguous `M_WAITOK` buffer, so it refused
> any new size past `TESSERA_WRITE_MATERIALIZE_MAX` (512 MiB). Lifted
> 2026-09-13 by `tessera_fs_extend_sparse`, which extends by appending hole
> windows through the existing append path (an all-zero chunk is a `ZERO_HOLE`
> record with no blob), under one flush gate so a crash cannot persist a
> half-extended file. Verified byte-exact across every starting layout, clean
> under fsck and after remount (`scripts/vm-sparse-extend-test.sh`), and under
> fsx with file sizes crossing the threshold.
>
> *What bounds it now* is the cost of creating the image, which is linear in
> its nominal size even though it is all holes — every hole chunk is a
> manifest record, and the flush gate is held for the whole extend:
>
> | image | truncate | metadata |
> |---|---|---|
> | 1 GiB | 0.25 s | 1.5 MiB |
> | 2 GiB | 0.52 s | 1.6 MiB |
> | 4 GiB | 0.99 s | 3.1 MiB |
> | 7 GiB | 1.76 s | 4.6 MiB |
>
> An 8 GiB quota needs a 10 GiB image: about 2.5 s during which every other
> publish on the shared store waits, once, at the app's first launch. That is
> the line; large media belongs in a user volume. The kmod also refuses to
> extend a file past its own volume's capacity (a hole still costs metadata),
> so the quota is further clamped to what the store can hold.
>
> *An existing image does not grow with the manifest.* Its inner filesystem was
> sized at creation, so launch mounts it at the quota it can actually hold and
> says so — the 480 MiB images built before the ceiling lifted still mount at
> 384 MiB. Getting a larger overlay means recreating the image.
>
> *Verified end to end:* a manifest asking for `4G` got a 5120 MiB image for
> 15 MiB of store space; `df` reported 4.0G; 3.5 GiB written through the union
> mount a jail uses left 512M available; the overlay volume fscked clean; the
> content matched after a relaunch; `portcullis remove` left nothing behind.
>
> *fsck had been flagging every overlay volume.* It counted quota usage only
> for files tagged with a domain, but the whole-volume default domain that
> `-o tessera.quota_bytes` creates charges UNTAGGED files — so any overlay with
> data reported "used_bytes=N but regular-file sizes sum to 0", and `--repair`
> would have zeroed `used_bytes`, handing the app back every byte it had
> already written as fresh quota. Fixed in the same change.
>
> **The cost figure above is for a FULL volume.** The image is
> allocate-on-write like any other file, so a fresh overlay costs its metadata,
> not its nominal size (measured: a 512 MiB image grew the store by 11.9 MiB, a
> 5120 MiB one by 15 MiB). Fifty idle apps is ~600 MiB, not 3.2 GiB; the ~6.4% applies to
> what each app actually stores. The fifty flush gates, GC contexts and
> pinscans are real either way.
>
> **The oracle claim was tested through the union, not just on the volume.**
> `unionfs` sums the layers for `statfs`: `f_blocks` is the lower layer plus
> the overlay (so a jail can see how big the pool is, which is static and not
> an oracle), but `f_bfree` and `f_bavail` — the numbers the oracle reads — come
> from the overlay alone. Measured field by field: 2097142 + 98304 = 2195446
> blocks through the union, with free and available both 98303, the overlay's
> own; neither moved when 64 MiB was written into the shared store. `df` inside
> the jail therefore shows a large and changing-looking *Used* column, but it is
> `f_blocks − f_bfree`, derived from the static total, not a view of the store. Copying a file that demonstrably exists on the shared volume
> (`libc.so.7` from the app tree, globally deduped) consumed exactly the same
> free space as writing the same number of random bytes: 2056 KiB both times.
>
> **Migrating an existing overlay is not automatic.** An app installed under
> the old layout has its state in the overlay *directory*, and mounting an
> empty volume over it would hide every file — indistinguishable from data
> loss. Launch detects a non-empty overlay directory with no image, leaves it
> a directory, arms `deferred`, and prints what to do. A slower shape, not an
> unsafe one.

Each overlay is provisioned inside its own quota domain
(tessera-quotas.md), which sets the dedup-domain boundary and
enforces the overlay's byte limit. atrium-volumes' tessera plugin
sets `dedup_policy` at domain creation.

The policy comes from the volume's own spec if it names one
(`dedup_policy` in the manifest's `[[volumes]]` entry), otherwise
from the backend-wide `dedup_policy` in
`/etc/atrium/volumes.policy.toml`, otherwise nothing is set and the
volume stays in whatever domain it inherits. Because the dedup
domain and the quota domain are one record, a volume that names a
policy gets a domain even with no `size_max`: the plugin mints it
with a limit of 0, which Tessera reads as unlimited.

> **Corrected 2026-09-12.** The sentence above was aspirational
> when written: the plugin did `mkdir` + `chown` +
> `TESSERA_IOC_QUOTA_SET` and nothing else, so every provisioned
> overlay came up `policy=global` — the oracle-open setting this
> section exists to avoid. Demonstrated live before the fix, with
> a freshly provisioned overlay landing as `domain 6
> root_inode=146 policy=global`. The `TESSERA_IOC_DEDUP_POLICY`
> plumbing now exists and the claim holds; a provision through a
> `dedup_policy = "deferred"` backend was verified to produce
> `domain 7 root_inode=148 policy=DEFERRED limit=0`.
>
> **Gap closed 2026-09-12.** `destroy` used to release the
> directory but not the quota-domain record, leaving it pointing
> at a freed inode. The table is capped at 256 domains and
> `TESSERA_IOC_QUOTA_SET` returns `ENOSPC` once it fills, at which
> point every later volume silently loses both its quota and its
> dedup policy — the plugin treats both ioctls as best-effort. The
> plugin now retires the domain through
> `TESSERA_IOC_QUOTA_DETACH` before removing the tree, and a mount
> reclaims records whose root directory is gone (four such
> orphans were reclaimed on the dev volume at first boot with the
> fix). Verified over provision/destroy cycles: the table returns
> to its live set each time.

Detaching a domain is safe only because domain ids are never
reused. Descendants of a retired tree still carry the old id in
their inode records, and with a reusing allocator that id could be
handed to an unrelated tree, silently charging those descendants to
it. The allocator is `next_quota_domain_id` in the superblock,
which only counts up. The format has always specified it as "the
monotonic allocator"; until this change nothing read or wrote it,
and the kmod derived each id from the highest one in the live
table — correct only while records are never removed.

> **Corrected 2026-09-12.** This paragraph previously also claimed
> the per-overlay domain "gives the jail quota-scoped `statfs`
> (tessera-quotas.md §3.6) — a jail never sees the pool-physical
> free-space counter." **That is false, and it was a security
> assurance**, so it is called out rather than quietly edited.
>
> §3.6 is explicit that scoping is **per-mount and cannot be
> per-path**: `VFS_STATFS(mp, sbp)` receives a *mount*, not a
> vnode, so the filesystem cannot know which directory the caller
> asked about. §3.6 records having made this exact mistake in an
> earlier draft. The implementation matches the spec — it keys on
> the *mount's* `quota_default_domain`, so a per-directory domain
> does not scope `df` at all.
>
> Measured on the apps volume, where `overlays/org.atrium.forum-bar`
> **is** a domain root with a 64 GiB quota — every path returns the
> identical whole-volume figures:
>
> ```
> /var/lib/atrium                                f_blocks=2097142 f_bavail=1959401
> /var/lib/atrium/apps                           f_blocks=2097142 f_bavail=1959401
> /var/lib/atrium/overlays                       f_blocks=2097142 f_bavail=1959401
> /var/lib/atrium/overlays/org.atrium.forum-bar  f_blocks=2097142 f_bavail=1959401
> ```
>
> So **`deferred` is the whole mitigation** for §20.1 channel 1 in
> this layout — nothing else is closing it, and the earlier wording
> invited the reader to assume a second, independent defence that
> does not exist. If genuine statfs scoping is wanted per jail, the
> overlay must be its **own mount** carrying a whole-FS quota
> (`mount -o tessera.quota_bytes=N`), which sets
> `quota_default_domain` and makes §3.6 apply. That is a layout
> change, not a policy flag.

### 4.2 Per-jail layout (split across three trees)

Per-app state is split across three sibling trees under
`/var/lib/atrium/`. `apps/` and `jails/` are subtrees of the shared
Tessera volume; each `overlays/<app.id>/` is its **own Tessera volume**
mounted at that path (adopted 2026-09-12, §4.1):

```
/var/lib/atrium/
├── apps/<app.id>/             ← lower layer: dedup'd app tree (Tessera)
│   ├── bin/atrium-edit            (read-only at launch; never mutated
│   ├── lib/...                     by the jail — preserves cross-jail
│   ├── share/...                   CAS dedup of binaries + libs)
│   └── atrium.toml
├── overlays/<app.id>/         ← upper layer: per-app writable — its OWN
│   │                             Tessera VOLUME mounted here with
│   │                             -o tessera.quota_bytes=N (adopted
│   │                             2026-09-12; see §4.1). Was a directory
│   │                             on the shared volume.
│   ├── home/                  ← what the app sees as $HOME
│   ├── tmp/                   ← scratch
│   ├── var/                   ← persisted app state
│   └── etc/                   ← per-instance config tweaks
└── jails/<app.id>/            ← unionfs mountpoint = jail.path
                                 (recreated each launch, torn down
                                 on jail exit; no persistent content
                                 of its own)
```

At launch, `portcullis launch --no-prompt`:

0. Mounts the app's overlay VOLUME at `overlays/<id>/` with its
   whole-FS quota, if not already mounted — creating the backing image
   and attaching it through `md(4)` on first launch:
   `mount -t tessera -o tessera.quota_bytes=N /dev/md<n> overlays/<id>/`.
   This is what makes the jail's `df` report its own quota rather than
   the pool (§3.6) — and therefore what closes §20.1 channel 1.
   Idempotent: a relaunch is one `statfs` and nothing else.
1. Mounts `apps/<id>/` read-only via nullfs at `jails/<id>/`.
2. Mounts `overlays/<id>/` writable via unionfs over the same
   `jails/<id>/`. Writes inside the jail land in the overlay;
   reads see the union.
3. Sets `jail.path = /var/lib/atrium/jails/<id>/` and runs
   `jail -c`.

On jail exit (or `jail -r`): tear down in reverse order — devfs,
unionfs, nullfs. The overlay VOLUME stays mounted across launches
(its content is the app's persistent state); it comes down only in
`portcullis remove`, which unmounts it, releases the `md(4)` device,
and deletes the backing image unless `--keep-overlay` says to keep it.
`rm -rf` on the overlay path is NOT a substitute: against a mounted
volume it empties the contents and then fails on the mount point, so
the state is destroyed while the volume and its image survive.

Rationale for the three-tree split (vs. nesting `rootfs/` and
`overlay/` under one per-app dir as earlier drafts suggested):

- `apps/` is what `tessera-import` writes; keeping it a flat
  tree of installed apps lets `portcullis launch <app-id>`
  resolve directly without knowing about overlay siblings.
- `overlays/` survives uninstall/reinstall cycles independently
  (state persists if the user reinstalls the same app id) and
  can be wiped per-app without touching the dedup'd rootfs.
- `jails/` is pure scratch — safe to `rm -rf` at any time when
  no jails are running.

Single-instance for now (one overlay per app id). Multi-instance
would key the overlay + jail dirs by an additional UUID; deferred
until there's a concrete app that needs it.

Inside the jail, the app sees a normal-looking root with:

```
/                ← the union (rootfs over overlay)
/atrium/sockets/ ← bind-mounted host sockets, per capabilities
/atrium/cas/     ← optional Tessera CAS read mount (system services only)
/dev             ← devfs limited by ruleset matching capabilities
/home/<user>     ← the per-app home (overlay/home from above)
/tmp             ← per-app tmp (overlay/tmp)
/usr/local/lib   ← populated by the app's first-run script (see §3.4) if it ran `pkg install` or similar; files dedup'd across jails by Tessera CAS automatically
/usr/local/include
/usr/local/share/...
```

### 4.3 Lifecycle vs. dedup safety

A common worry: "if I close app A, do its files (which app B
might be sharing via dedup) disappear?"

Answer: **no, never accidentally.** The mechanics:

| Event | Mounts | A's subtree | Shared blob refs |
|---|---|---|---|
| App A jail stopped (`jail -r`) | Unmounted | Persists in Tessera | Unchanged (A still references) |
| App A jail re-launched | Re-mounted | Same files visible | Unchanged |
| App A uninstalled (`portcullis remove`) | Already stopped | Deleted (subtree removed from Tessera) | Blobs reachable only via A's manifests become GC-eligible |
| App B uninstalled later | — | Deleted | Blobs reachable only via B's manifests → GC eventually reclaims |

The key invariant: **Tessera GC only reclaims blobs unreachable
from every live inode's manifest and every pinned GC root** —
mark-sweep reachability, not on-disk refcounts (tessera-fs.md
§11/§15; the observable semantics are refcount-like, the
mechanism is not). A blob still reachable from ANY live inode (in
any subtree on the volume) cannot be reclaimed. So:

- Stopping a jail = unmounting a view; the underlying data is
  unchanged. Other jails are unaffected.
- Uninstalling an app = deleting its subtree; only blobs that
  were unique to that app become reclaimable.
- Shared blobs (`/usr/local/lib/libssl.so.3` referenced by both
  A's and B's pkg-installed copies) remain on disk as long as
  ANY app references them.

This works because Tessera's GC walks the live-inode set and
marks all reachable packs; unreachable packs are reclaimed.
There is no path by which "app A stops" can remove a blob that
"app B is using."

(Tested behaviour, not theoretical — `tessera-import` re-import
measurements land 9.6× dedup with both source trees intact;
`data_gc_test` in `scratch/` covers GC correctness.)

## 5. Capability → jail config translation

Each capability is a small function `apply_<cap>(jail, manifest_value) -> ()`
that emits the corresponding jail.conf fragments. Mechanical, no
policy logic.

Sample translations:

```
clipboard = true
  →  mount.nullfs += "/atrium/sockets/clipboard.sock /atrium/sockets/clipboard.sock"

filesystem = ["~/Documents"]
  →  mount.nullfs += "$USER_HOME/Documents /home/$USER/Documents"
     (mode preserved; rw by default)

graphics = "fresco"
  →  mount.nullfs += "/atrium/sockets/fresco.sock /atrium/sockets/fresco.sock"
  →  devfs ruleset includes /dev/fresco0

network = "none"
  →  ip4 = disable
     ip6 = disable
     allow.raw_sockets = 0
     vnet = inherit-none

network = "loopback"
  →  ip4.addr = 127.0.0.<jail-loopback-id>/8
     ip6.addr = ::1
     vnet = new

network = "full"
  →  vnet = inherit
     (the host's default interface is reachable; outbound
      filtering by pf if configured)

[setup] + [setup.capabilities]
  →  on first launch (no .atrium-firstrun-done sentinel):
       compute effective_caps = [capabilities] ⊕ [setup.capabilities]
       apply each effective cap via the same per-cap translators
         used at runtime (uniform machinery)
       exec.created = setup.command (with setup.timeout enforced)
       on success: write sentinel; tear down the elevated jail.
  →  every launch (incl. post-setup):
       apply runtime [capabilities] only
       exec.start = app.entry
```

The complete table lives in `portcullis/src/capabilities.rs`
alongside the parser, with one test per row.

## 6. Jail lifecycle

```
┌─────────────────────────────────────────────────────────────┐
│ 1. portcullis launch <app-id>                              │
│    or  portcullis launch <path-to-tessera-imported-tree>   │
└─────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│ 2. Resolve app tree                                        │
│    - app.id → /var/lib/atrium/apps/<id>/ (managed Tessera) │
│    - or direct tree path for development                   │
└─────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│ 3. Parse + validate atrium.toml                            │
└─────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│ 4. Capability policy check                                 │
│    - For each capability: is it in the user's grant list?  │
│    - If not, send prompt to portcullisd; block until reply │
│    - On Deny: fail launch with EACCES + a clear message    │
│    - On Allow: persist grant (one-shot or persistent)      │
└─────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│ 5. Build runtime.conf                                      │
│    - apply_<cap> for each granted capability               │
│    - emit a single jail.conf section under the app's id    │
│    - validate with `jail -c -f runtime.conf -n`            │
└─────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│ 6. Mount overlay union (if not already)                    │
│    - union of rootfs (Tessera, ro) over overlay (Tessera, rw)│
│    - cached across launches; idempotent                    │
└─────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│ 7. jail -c -f runtime.conf                                 │
│    - enters the jail                                       │
│    - exec.start runs /etc/rc which starts everything        │
│      enabled in the jail's rc.conf (helpers, etc.)         │
│    - then execs app.entry as the foreground PID            │
│    - returns the jail's pid                                │
└─────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│ 8. portcullisd supervises                                  │
│    - waitpid(entry-pid)                                    │
│    - on entry exit:                                        │
│        per supervision policy: do nothing, restart, log    │
│        jail -r runs /etc/rc.shutdown which stops services  │
│        in reverse order, then destroys the jail            │
│    - on app-id reuse mid-session: per atrium.toml          │
│      [supervision].instances policy                        │
└─────────────────────────────────────────────────────────────┘
```

## 6.5 One-shot piped jails — a jail per unit of work

Everything above assumes a jail is an **application**: one per app id, long-lived, attached
to a persistent overlay, single-instance by default. That is the right shape for a notes
app and the wrong shape for a **worker pool**.

The Navigator's backend needs one jailed document worker *per document*
([atrium-navigator-backend.md](atrium-navigator-backend.md) §2: "one jail per document is
the default, not a mitigation"), talking over a pipe, living exactly as long as the document
is open. Sixteen open pages are sixteen concurrent jails **of the same app**. Three things
in the current design stand in the way.

### 6.5.1 Names must carry an instance — and the failure was silent

`jail_name_from_app_id` derives the name from the app id alone. Two concurrent workers get
the same name, and **`jail -c` on an existing name does not fail — it reconfigures the
running jail**. Two documents would have ended up inside one jail, sharing the boundary each
was supposed to have to itself, with nothing logged and every test green.

`BuildOpts::instance` fixes this: `Some(tag)` yields `<id>__<tag>`, `None` reproduces the
old name byte-for-byte so no existing launch changes. The tag is sanitized to
`[A-Za-z0-9_]`, because **FreeBSD reads a dot in a jail name as hierarchy** (`a.b` is a
child jail of `a`) — a tag taken from a url, a uuid or a path would turn a naming
convenience into a nesting bug. The **hostname does not take the tag**: it is what the app
sees of itself, and a document worker should not be able to read which slot it was given.

*Implemented, with tests (`portcullis-jail/tests/instances.rs`).*

### 6.5.2 `portcullis exec` — the piped one-shot launch path

**Implemented and verified in the FreeBSD VM.** `portcullis exec [--instance <tag>]
[--tmpfs-size <n>] <app-id|app-tree>` runs an app's entry in a one-shot jail whose
stdin/stdout/stderr are the calling process's own — so a broker that spawned it with pipes
talks to the jailed process directly.

Four differences from `launch`, each deliberate:

- **Per-instance name and root** (§6.5.1), so many run concurrently from one app.
- **No persistent overlay.** The writable layer is tmpfs, discarded with the jail. A worker
  holds nothing worth keeping, and a lane that churns jails must not accumulate on-disk
  overlays for something else to garbage-collect.
- **Signatures are `Demand::Required`** (§6.5.3), not merely default-checked.
- **A live jail with the same name is refused, not cleaned up.**

Stdio needed no new mechanism: `jail -c -f` inherits the caller's descriptors.

**Three things only running it revealed.**

1. **tmpfs on the jail root MASKS the tree, it does not layer over it.** The first version
   mounted the read-only nullfs and then tmpfs at the same point; every mount succeeded, and
   the jail booted with an empty root and `exec /bin/sh: No such file or directory`. The
   upper layer must be mounted *outside* the root and **unionfs**'d over it. A stacking
   filesystem and a second mount at the same mountpoint are indistinguishable in `mount -p`.
2. **jail(8) chdirs into the run user's home inside the jail**, taken from the host's passwd
   because `exec.system_jail_user` is set. Without that directory existing in the jail's
   namespace, `exec.start` fails before the entry runs. It is created empty in the tmpfs
   layer.
3. **A duplicate instance tag killed a live reader.** The pre-teardown exists to recover the
   leftovers of a run that *died* — stacking a fresh nullfs on an abandoned pile is how a
   mount stack becomes unrecoverable without a reboot — but it cannot tell a dead jail from
   a running one, and measured against a live one it sent the first reader's process
   SIGTERM and then failed anyway. Both lost, for a reason neither could act on. Liveness is
   now checked with `jls` first and a duplicate is refused, naming the jid.

**One fidelity limit, stated rather than implied:** `jail(8)` collapses every nonzero
`exec.start` status to 1, so `exec` reports success or failure and **not** the child's exit
code. Measured, not assumed: a child exiting 7 makes `jail(8)` exit 1.

**Verified in the VM** (FreeBSD 16.0-CURRENT, aarch64, cross-built on the host):

| | result |
|---|---|
| unsigned manifest, no publishers | refused — `Demand::Required` holds |
| signed manifest, publisher installed | verified, jail created, entry ran |
| pipe round trip | host `stdin` → jailed process → host `stdout` |
| two concurrent instances | two live jails, distinct names and roots |
| duplicate live tag | refused, first jail unharmed |
| host filesystem visible to the jail | `/etc`, `/usr`, `/var`, `/home` absent; only the tree, `/dev`, and the created home |
| writes | land in tmpfs, gone on the next run; the app tree is untouched |
| stale mounts from a killed run | recovered and cleaned |
| after exit | no jails, no mounts, no roots, no upper dirs |

**Still open:** this is the CLI path. `portcullisd` integration (so the broker asks the
daemon rather than spawning a setuid-ish CLI) and the `jaild` `CreateJail`/`ExecSpec` route
with caller-supplied fds — real `execve`, `pdfork` reaping, no intervening `/bin/sh` — remain
as described in §6.5.4.

### 6.5.4 The one-shot lane runs on `jaild` — DONE (2026-09-22)

`jail -c -f` started the entry through `/bin/sh -c` via `exec.start` — a shell in every jail,
argv quoting as the caller's problem, `-q` load-bearing so jail(8)'s chatter stayed off the
caller's pipe, and every nonzero exit collapsed to 1. The lane now asks **jaild**:
`portcullis-oneshot` still builds the root (read-only tree, tmpfs upper, union) and then
sends `CreateJail` with `exec.stdio = true` and the caller's three descriptors riding on the
same `sendmsg`. jaild `pdfork`s, mounts the jail's devfs (ruleset 22, checked loaded —
§9.1b), `dup2`s the descriptors onto 0/1/2, `jail_set(CREATE|ATTACH)`s, drops privilege
(verified — §9.1a) and `execve`s the entry. It returns a procdesc; the lane waits on it with
`EVFILT_PROCDESC` (an already-exited worker reports at registration — `sys_procdesc.c`, no
race) and reports the worker's **own** exit status, then sends `RemoveJail` so jaild's state
does not keep one record per worker.

**Two decisions, made explicitly:**

- **Entry path trust = the verified tree.** jaild execs service binaries only under
  `exec_paths.allowed_prefixes`; a signed bundle's entry is wherever its manifest says. So
  jaild trusts an entry anywhere inside the tree **only** for a one-shot *instance root*:
  a jail named `app-…` whose root is **exactly** `<exec_paths.instance_root_dir>/<name>`
  (component-wise equality; a deeper path or another jail's root gets no trust) — and the
  path must still be absolute with no `..`. Only an instance root may take the caller's
  stdio. Names are `app-<id>--<tag>` (`portcullis_jail::jaild_instance_name`), jaild-valid by
  construction; too long is refused, never truncated.
- **Root callers are refused.** A worker runs as whoever asked for it, and a root caller
  meant a uid-0 worker — what turned §9.1b's `/dev` exposure into a read of the host's disk.
  Refused in the lane with the reason (jaild's uid policy would refuse it too). The daemon
  path uses the peer's credentials; the direct CLI needs root and so requires an explicit
  `--user <name>` — never `$USER`, which `su -m` leaves as root.

**What the lane cannot express is refused, not dropped:** capability device grants and any
network. A capability that silently did not apply would be a worker running without what its
manifest says it has.

**Descriptors on a TCB socket.** `sockmux` (jaild's multiplexer) receives SCM_RIGHTS now:
close-on-exec from receipt (`MSG_CMSG_CLOEXEC`), bounded per connection (3 for jaild, 0 for
atrium-volumes), assigned to the frame whose **bytes** they arrived with (a pipelined plain
request cannot take the next one's), and never silently dropped — a frame taken as plain that
carried descriptors is an error. jaild refuses descriptors on any request but a stdio exec
(`fds.unexpected`) and anything but exactly three there (`exec.stdio.fd_count`).

**Found on the way, fixed:** the procdesc was received **without** close-on-exec (jaild's
client) and created **without** `PD_CLOEXEC` (jaild). A caller serving several one-shots
forks `mount`/`umount`/`jls` for each, and every such child inherited the other workers'
procdescs — and a zombie is reaped only when its *last* descriptor closes. Both ends are
close-on-exec now.

**Measured in the VM,** broker as uid 1001 (`navtest`), through portcullisd:

| check | result |
|---|---|
| corpus | 98 sessions / 259 navs / 259 rewinds / 0 failures, 58.4 s (jail(8) lane: 59.2 s) |
| leaks | no jails, mounts or upper dirs; jaild state: 0 worker records — **but see the correction below: every worker was left a zombie holding a `dying` jail** |
| refusals | unsigned, duplicate live instance, **root caller** — all refused |
| exit status | malformed frame → CLI exits **2** (the worker's code); jail(8) reported 1 |
| process | jail on devfs ruleset 22; parent of the worker is `atrium-jaild` — no `/bin/sh` |

**★★★ CORRECTION (same day) — the "0 zombies" above was a DEAD ARM, and the leak was real.**
The counter was `ps -o ppid=,stat=`: with `=` the comma-list is ONE column, so `$2` never
existed and the check could only print 0. The first observation — ~100 zombies under jaild,
read with a correctly-formed `ps` — was the true one; "they did not reproduce" was the broken
counter. They reproduced on every run, and each zombie held its jail in `dying` (`jls`
without `-d` does not list those, so the harness's leak check passed too).

**Cause: a resync behaviour change.** Upstream `bcdb6ba94d08` (2026-07-15, "processes: add
zombie references") made a `pdfork` child need BOTH its procdesc closed AND its parent's
`waitpid()` before it is reaped — unless forked with `PD_NOWAITPID`. jaild gives the
procdesc holder the whole lifecycle and never waits, so after the resync every child it made
(every one-shot worker, every service restart) stayed a zombie. Closing the last procdesc of a
zombie was observed NOT to reap it (`jclient` holding the only procdesc, then exiting). **Fix:**
`pdfork(PD_CLOEXEC | PD_NOWAITPID)` — the flag that states jaild's actual contract; libcasper
adopted it upstream for the same reason — with a fallback for kernels that predate the flag
(where closing already reaps). **Verified:** counter positive-controlled (a deliberate zombie
reads 1); after the fix, jclient execs and a full corpus run leave **0 zombies, 0 dying
jails**. The harness now fails on dying jails and on jaild zombies.

The close-on-exec fixes above stand on their own (a procdesc inherited by an unrelated
command is a real extra holder), but they were not the cause.

### 6.5.2a `Request::ExecInstance` — the daemon creates the jail

`portcullis exec --daemon` asks **portcullisd** to create the jail and hands it the caller's
three descriptors over `SCM_RIGHTS` — the same handshake `Launch` already uses
(`ReadyForFds` → `send_fds` → `LaunchExit`). The calling process creates no jail, mounts
nothing, and needs no privilege.

**A separate request, not a flag on `Launch`.** They are different lifecycles, not variants
of one. `Launch` is an *application*: one jail per app id, a persistent overlay, a dedicated
per-app uid, first-run setup, single-instance. `ExecInstance` is a *unit of work*: a
per-instance jail and root, a tmpfs upper layer discarded at exit, no overlay, no first-run,
many concurrently from one app. Folding them together would put a boolean in the middle of
the launch path deciding which half of itself to skip.

**One implementation, two callers.** The operation lives in `portcullis-oneshot`; the CLI
parses arguments into it and the daemon calls it after receiving the descriptors. The trust
gate already taught this tree what three copies of one decision cost (§6.5.3), so the
daemon does not get its own.

`--daemon` **does not fall back.** If portcullisd is not running it refuses, because
quietly creating the jail in the calling process would grant exactly the privilege the
caller asked to avoid — and would do it without saying so.

**The run user's home is resolved from passwd, not supplied.** `exec.system_jail_user` makes
jail(8) chdir into that user's passwd home *inside* the jail, so any other answer creates
the wrong directory and the entry dies before it runs. The CLI previously passed `$HOME`
(right only because root's `$HOME` happens to match) and the daemon's first version
constructed `/home/<user>` (simply wrong — `chdir /root: No such file or directory`).

**Measured in the VM**, with the Navigator's 98-recording corpus, one jail per document:

| broker runs as | jails created by | sessions | navs | rewinds | failures |
|---|---|---|---|---|---|
| root | the CLI itself | 98 | 259 | 259 | 0 |
| root | **portcullisd** | 98 | 259 | 259 | 0 |
| **uid 1001, unprivileged** | **portcullisd** | 98 | 259 | 259 | 0 |

No jails, mounts or roots left behind in any row, and the daemon survived all three. The
last row is the point: the same unprivileged user, asked to create a jail directly, is
refused — `mkdir /var/lib/atrium/jails/…: Permission denied`. The privilege is in the
daemon, and that is now a demonstrated property rather than a described one.

### 6.5.2b Rate and concurrency limits on `ExecInstance`

**`ExecInstance` is the first daemon verb a program calls in a loop.** `Launch` is driven by
a person clicking something; a worker pool is driven by pages opening. Nothing on that path
restrained it: a client could ask for jails as fast as the daemon could make them, and the
only thing in the way was the *client's* own session bound — a limit held by the thing being
limited.

Two exhaustions, so two limits:

- **Concurrency** bounds what exists at once — each one-shot jail carries a tmpfs and a mount
  stack. Per user (32) with a host ceiling behind it (64): per-user alone multiplies by
  adding users, global alone lets one client starve everyone.
- **Rate** bounds churn. A client that creates and destroys in a tight loop holds almost
  nothing at any instant and still saturates the daemon, `jail(8)` and the mount table. A
  token bucket: burst 32, sustained 8/s.

**Derived from measurement.** The Navigator's corpus creates 98 jails back to back in 58.7s
— **1.67 jails/second** sustained, bursting to 16 when a broker opens every session it is
allowed. The defaults are several times that and orders of magnitude below what a loop asks
for.

Three details that are the difference between a limit and a nuisance:

- **Limited before the fd handshake.** Answering `ReadyForFds` first would have the client
  send three descriptors the daemon is about to refuse — a refusal that still costs the
  caller work is one a loop can use as a service.
- **A refusal does not spend a token.** A client at its concurrency limit that retries would
  otherwise exhaust its rate budget too, punished twice for one condition.
- **The slot is an RAII guard.** A leaked count is permanent — it lowers the limit for the
  daemon's life with nothing to show why, and the machine slowly refuses work it could do.
  Tested against a panicking holder.

The clock is monotonic, not wall: a rate limit measured against a clock NTP can step
backwards is one that can be widened by changing the time.

**Measured in the VM:**

| | result |
|---|---|
| the 98-document corpus | **unaffected** — 98/259/259, same 58.7s |
| 60-iteration create/destroy loop | 38 ran, **22 rate-limited** |
| 40 concurrent requests, limit 32 | **exactly 8 refused**, per-user limit named |
| after all of it | no jails, no mounts, daemon alive |

### 6.5.2c The policy question for the one-shot lane, settled

`ExecInstance` skipped the policy gate entirely, leaving the signature as the only check. So
a signed manifest declaring `network = "full"` and `filesystem = ["~/Documents"]` would have
received all of it, unasked, on a path built for workers that need nothing — installing a
signed app was effectively granting it everything it declared, provided it was launched this
way rather than the other.

**Settled: no prompt, and no ungranted capability either.**

Both obvious answers are wrong:

- **Prompt, like `Launch`.** There is nobody to ask. A broker opens sixteen workers because
  sixteen pages are open; a tty prompt per worker is not a consent mechanism, it is a hang.
  And "non-tty gets a refusal" means the lane cannot work at all.
- **Require the manifest to declare nothing.** Tempting — that is the case the lane was
  built for — but a rendering worker legitimately wants the font set, and forbidding it
  forces every future worker back onto the application path it does not fit.

So the delta is computed exactly as `Launch` computes it, and a non-empty one is a
**refusal** rather than a prompt, naming what must be granted and how. A worker declaring
nothing has an empty delta and runs with **no setup at all**; a worker that wants something
gets it only after a human has already said yes through `portcullis policy grant`.

**There is no `bypass_policy` here, deliberately.** `Launch` has one for development
(`--no-prompt`). On a path a program drives in a loop, a bypass flag is not a developer
convenience but a permanent hole with a friendly name.

The gate runs **before the fd handshake**, like the rate limit: a refusal that first makes
the client hand over three descriptors has charged it for nothing.

**This also exposed an unimplemented step.** Granting the capabilities got the greedy worker
past policy and straight into `mount: …/home: No such file or directory` — the one-shot path
never created mountpoints, which `jail(8)` does not do for itself. Without that fix the lane
would have supported capability-bearing workers only in principle, and the argument for
allowing them would have collapsed into "capability-less only, by accident of a missing
step". Dir-or-file is decided by stat'ing the source, as the application path does.

**Measured in the VM:**

| | result |
|---|---|
| capability-less worker | runs, zero setup |
| signed worker wanting network + `~/Documents` | **refused**, both capabilities named, with the grant command |
| same worker after `policy grant` | runs |
| after `policy revoke` | refused again |
| the 98-document corpus | unaffected — 98/259/259 |

### 6.5.2d One copy of the mount mechanics

Three lanes had three copies of "unmount everything under this root", at **three different
qualities** — which is worse than three identical ones, because the weakest was on the path
nobody watches:

| lane | what it had |
|---|---|
| `portcullisd` application launch | the good one: re-enumerate each pass, stop on no progress, warn about survivors |
| the one-shot lane | a near-copy: convergent, but spun all 16 passes, and knew about a second root the others did not |
| the CLI's local fallback | **the original**: unmount `dev`, then the jail path twice, and hope |

The first got that way by being debugged — capability mounts live *under* the jail path, so
they held the overlay busy and every relaunch stacked a fresh set on the survivors (12
mounts with no jail, no process, no open file). That fix never reached the CLI fallback,
which is taken only when the daemon is down.

`portcullis-mounts` is their **union, not their intersection**. Every behaviour any of them
had is kept, and the two differences that were real became parameters rather than being
averaged away:

- **Force.** A one-shot worker's jail is gone by teardown time and nothing should hold its
  mounts, so forcing costs nothing and guarantees the next run does not stack. An
  application's mounts may be genuinely busy, and forcing there takes a filesystem away from
  something still using it — that path asks politely and reports what survived.
- **Multiple roots.** The one-shot writable layer is mounted outside the jail root; sweeping
  only the root left one tmpfs per run alive in `/var/run`.

The parsing is separated from running `mount(8)` so it can be tested on a host with none of
these filesystems — and the subtle part is the prefix rule, not the subprocess:
`Path::starts_with` is component-wise, so `…/app-sibling` is **not** under `…/app`. A string
prefix test would have swept another jail's stack into this one's teardown. That case is now
a test.

Verified in the VM after the merge: the 98-document corpus through daemon-created jails
(98/259/259), the direct CLI lane, and stale-mount recovery all unchanged, with no jails,
mounts or upper directories left behind.

### 6.5.2f `persist = false` — the husk, fixed at the cause

`jail -c` creates with `persist = true` because the application path needs the jail to
outlive nothing in particular: jail(8) holds it while `exec.start` runs and the launcher
removes it afterwards. For a **unit of work** that is wrong, and the wrongness had already
been patched around twice before the cause was addressed:

- A launcher that is **killed** never reaches its teardown, and the kernel keeps a named,
  process-less jail forever. That husk poisons its instance tag, so the next worker with
  that tag is refused (§6.5.2) — patch one: reclaim a process-less jail instead of refusing.
- `memfed` then discovered the husks as pool members, budgeted them, and pinned rctl rules
  to them — and **a rule outlives the husk**, so the next worker reusing the tag inherits a
  stranger's cap (§6.5.2e) — patch two: exclude zero-RSS members.

`BuildOpts::persist` makes it a choice, and the one-shot lane takes `false`. The jail is
then removed the moment its last process exits, so killing the launcher cleans up **by
construction**: the worker sees EOF on the pipe that died with its parent, exits, and the
jail goes with it. Measured on the same machine that produced the husks — after a SIGKILL of
the launcher, with no teardown running at all, the jail was gone once its process ended.

Both earlier patches stay. They are no longer the only defence, but a jail whose worker is
still running when its launcher dies is a real state, and reclaiming rather than refusing is
still the right answer for it.

**What `persist = false` does NOT fix: the mounts.** A SIGKILLed launcher still leaves its
nullfs/tmpfs/unionfs stack — measured at 24 mounts — because teardown is the only thing that
unwinds them. That is what the pre-run convergent teardown (§6.5.2) exists for, and it is
why that pre-teardown is not redundant with this change.

### 6.5.2e Memory limits for ephemeral jails — an integration gap, not a missing knob

Bounding how *many* one-shot jails exist (§6.5.2b) says nothing about how much memory any
one of them may take. The obvious fix — pin a static `rctl` cap in this lane — is wrong, and
the reason is worth recording because it is not obvious from inside the lane.

**`memoryuse` is RSS, and RSS can only be enforced by killing.** You cannot cleanly fail a
page fault, so rctl offers `sigkill`/`sigterm` for it and `deny` only for virtual and swap
([atrium-memory-pressure.md](atrium-memory-pressure.md)). There is no soft version of this
knob.

**And Atrium already has the adaptive answer.** `memfed` water-fills RAM across jails by
weight and pushes each one's `memoryuse` cap dynamically, **never below current RSS**, so an
over-budget jail is *frozen rather than killed* — and it acts through the jaild broker,
because a jailed governor cannot rctl a sibling. A constant pinned by this lane would not
merely duplicate that; it would **fight** it, killing a worker the federation would have
spared.

**So the real gap is integration, not a missing limit:** `memfed` budgets jails **by name**
from operator configuration, and one-shot worker jails have ephemeral names
(`<id>__<instance>`) that no configuration can enumerate. Ephemeral jails are therefore
outside the memory federation entirely — they are neither budgeted by it nor visible to it.

Two ways to close it, both larger than a flag:

1. **Register ephemeral jails with the federation** — have the daemon that creates them tell
   `memfed` (weight, lifecycle tier), so a worker pool is budgeted as a pool rather than as
   an unbounded set of strangers.
2. **Create them through `jaild`** (§6.5.4), after which they are jails jaild knows and
   `SetRctl` applies to them like any other — which also removes the direct `rctl(8)`
   shellout this lane would otherwise need.

Until then `--memory <MiB>` exists as a deliberate, **opt-in** safety net for a deployment
whose jails the federation cannot see, and `--require-memory-limit` refuses to run uncapped
rather than pretending. Note that `kern.racct.enable` is a **loader tunable**: a machine that
did not boot with it cannot enforce any cap until it reboots, so this is a fact to report
and never something to switch on underneath an operator.

### 6.5.3 The trust gate: `require_signatures`

Two findings surfaced while designing the worker lane. Neither was caused by it; both were
load-bearing for it, and both are now fixed.

**1. Manifest trust failed open when unconfigured.** `verify` warned and returned `Ok` when
`/etc/atrium/publishers` was empty — the default on a fresh machine. Defensible for a
developer box, where enforcement begins the moment the first key is installed; indefensible
for a lane that launches jails continuously, where an unsigned-manifest window stops being a
one-off and becomes a standing condition.

Two mechanisms now, because these are two different questions:

| | who decides | where |
|---|---|---|
| `require_signatures` | the **operator** | `/etc/atrium/trust.toml`, default `false` |
| `Demand::Required` | the **caller** | per launch, in code |

A caller can demand more than the operator configured; **it can never demand less.** The
jailed-worker lane will pass `Demand::Required`, so a worker pool cannot run unsigned even
on a machine that still allows it for ordinary apps.

The default stays `false` so upgrading a machine does not change its behaviour — flipping it
would stop every developer box from launching anything, which is a decision an operator
takes, not a side effect of a release.

**A malformed `trust.toml` fails closed.** An absent file is a decision (the documented
default); an unparsable one is an accident, and reading an accident as permission is the same
fail-open bug one level up — silently, since the machine would keep launching exactly as
before. A file that parses but omits the key keeps the default; only a broken one is treated
as an error.

And the setting governs the **unconfigured case only**. Once publisher keys are installed, an
unsigned manifest is refused whatever `require_signatures` says: the setting must never become
a way to weaken a configured machine.

**2. Two other launch vectors disagreed with the gate — and with each other.** The module
claimed to be "shared by every user-app launch vector so the check is uniform, not copied per
path", while living private inside the daemon binary. In fact `atrium-launch` carried a second
copy that **refused** on empty publishers, the daemon's **allowed**, and the CLI's local
fallback (explicit path, daemon offline) had **no gate at all** — it built and ran `jail -c`
from whatever `atrium.toml` sat at the given path.

The gate is now its own crate, `portcullis-trust`, reachable by all three. `atrium-launch`
passes `Demand::Required`, keeping its stricter behaviour deliberately rather than as an
accident of having been written separately; the CLI fallback passes `Demand::PolicyDefault`,
so an unconfigured machine behaves exactly as before and an operator who sets
`require_signatures` gets it enforced there too. A module that says it is shared has to be
reachable by the things that must share it.

## 7. Capability policy + prompts

User policy at `/var/db/atrium/<user>/policy.toml`:

```toml
# Per-user persistent grants. Editable but normally managed by
# portcullisd in response to user prompts.
[grants."org.atrium.edit"]
graphics    = "fresco"     # granted on first launch
clipboard   = true
notify      = true
filesystem  = ["~/Documents", "~/Projects"]
network     = "none"

# Recorded so a manifest CHANGE forces a re-prompt.
manifest_hash = "a1b2c3d4..."
granted_at    = "2026-04-15T10:30:00Z"
```

`portcullisd` workflow on first launch (or manifest change):
1. App spawns from Forum: `portcullis launch org.atrium.edit`.
2. portcullis-cli connects to portcullisd via /atrium/sockets/portcullis.sock.
3. portcullisd computes `delta = (manifest_caps - granted_caps)`.
4. If `delta` is empty: launch immediately (return success to portcullis-cli).
5. If non-empty: send a prompt message via aqueduct to a UI service
   (Forum or a dedicated `atrium-prompt` daemon). UI presents:
   ```
   "Atrium Edit" wants to:
       ✓ Read/write your Documents folder      [explanation]
       ✓ Show desktop notifications            [explanation]
       ✓ Send things to clipboard              [explanation]
   [Allow once]   [Allow always]   [Deny]
   ```
6. UI replies; portcullisd persists the grant per choice (`once`
   keeps it for this session only; `always` writes to policy.toml);
   then launches OR returns EACCES.

The CLI dev mode (`--allow-all`) bypasses prompts for development.
A trusted-installer mode (`--policy /etc/atrium/policy.toml`)
pre-grants for headless deployments.

## 8. Integration with existing Atrium pieces

### 8.1 Tessera

- App trees are imported via `tessera-import` into a managed
  location (`/var/lib/atrium/apps/<id>/tree`). Cross-jail dedup
  is automatic — two apps that share libssl share the underlying
  CAS blobs.
- The jail's rootfs is a Tessera mount with `tessera.gen=N` set
  to the version installed (snapshots make rollback trivial).
- The overlay is Tessera too — per-app writable, snapshottable,
  garbage-collected when the app is uninstalled.

### 8.2 aqueduct

- The capability boundary IS the substrate aqueduct was
  designed for. Each `<service> = true` line in the manifest
  becomes one nullfs mount of one socket. Apps without the
  capability literally cannot see the socket — aqueduct's
  "filesystem-as-capability" property is enforced by the
  kernel mount table.
- `tessera-cas-read = true` is the special trusted-service
  capability that grants read of the global CAS. Apps don't get
  this; system services do, by manual policy.

### 8.3 D1.7 binsplit (when it lands)

- `tessera-import --binsplit` extracts function blobs at install
  time. The app tree's `bin/` directory contains recipe files;
  reconstitution materialises into a per-app cache at first
  launch.
- Portcullis doesn't need to know about binsplit at the launch
  layer — by the time we're launching, the binary is already
  materialised. It just sees a normal ELF.
- Per-app materialisation cache lives in the overlay (`overlay/
  state/binsplit-cache/`) so it's snapshotted/gc'd along with
  everything else.

### 8.4 Forum (D3) and Vestibulum (D2)

- Vestibulum (login) launches the per-user session: starts
  portcullisd, starts user-scoped services (clipboard, notify),
  starts Forum.
- Forum is the user's "shell" — wallpaper, status bar, dock.
  The dock reads `/var/lib/atrium/apps/*/atrium.toml` and shows
  an icon per app. Click → `portcullis launch <id>`.
- portcullisd mediates the prompts.

## 9. Security model

### 9.0 Privilege invariant — only the TCB runs as root

**No application ever runs as root. Root (uid 0) is reserved for the TCB:
`jaild` (and its policy file) at TCB rank 1, and the privileged side of the
launch path. Everything else — every system service AND every user app — runs
unprivileged inside a jail under a *dedicated, non-root uid*.**

This rests on a deeper split. Traditional Unix **fuses** identity, execution,
and authorization into one uid (you log in *as* uid N, your processes *run as*
uid N, and N's permissions *authorize* them). Atrium **splits** the three:

- **identity / ownership = the human uid** (a real login user). The human
  *authenticates* (PAM auth/account) and *owns* data + the policy file. A human
  uid is a principal, **not** something processes run as — "PAM says who walked
  in; jaild + seat + capabilities decide what running as them means."
- **execution = a non-human, per-app uid** (the dedicated 50000+ range). This is
  what app processes actually run as: unprivileged, distinct per app, a "nobody"
  that owns nothing on its own.
- **authorization = capabilities** (the manifest grants), not uid permissions.

So there are four uid classes, and only one runs apps:

| class | range | role | runs app processes? |
|-------|-------|------|---------------------|
| root | 0 | the TCB (jaild + privileged launch step) | no — TCB only |
| human / owner | real login range (e.g. 1000–49999) | identity, ownership, policy | **no** — owner, not runner |
| per-app | 50000+ (dedicated, one per app) | **execution** | **yes** |
| system service | specific `allowed_system_uids` (e.g. `_frescod`) | blessed engines | yes |

The consequence that bites: an app must run as a **per-app (non-human) uid** —
not root **and not the human's own uid**. jaild's range check (1000–65000)
refuses root but would happily accept a *human's* uid; running as a dedicated
50000+ app uid is the additional discipline the launch path must apply.

Concretely:
- **The TCB** (`jaild`, the root of trust; the mount/`jail -c`/`setuid` step) is
  the *only* code that holds root, and it holds it precisely so it can drop it:
  jaild creates the jail and `setuid`s the target to a non-root uid before
  `execve`. This is the whole point of the OpenSSH/qmail-style privsep (§0.5).
- **Every app** runs under a per-app uid in the user range (jaild policy
  `[uid].min_user_uid..max_user_uid`, 1000–65000; see `jaild-policy.md`). jaild
  *refuses* an app launch with `uid = root` — root is permitted only for the
  specific blessed system uids in `[uid].allowed_system_uids`, never for a
  third-party app. A per-app uid (distinct from the human user and from every
  other app) is what gives §9.1's app-to-app isolation its identity dimension:
  it bounds jail-escape blast radius, owns the app's shared-resource access, and
  is what services peer-cred (`getpeereid`) back to an app id. See the app-
  isolation model (jail + dedicated uid as complementary layers).
- **The human user authorizes; the app does not run *as* the human.** The
  connecting user's policy (`/var/db/atrium/<user>/policy.toml`) decides *whether*
  to launch and with which caps; the launched process then runs as its own
  per-app uid, not the human's uid and never root.

> ⚠️ **Known deviation (bring-up, 2026-06-16).** The current `portcullisd`
> launch path (`launch.rs`, the legacy `jail(8)` subprocess route — NOT yet the
> jaild route) sets the jail's `exec.jail_user` to the *connecting user* rather
> than allocating a per-app uid. When the launch is driven by a root caller (e.g.
> the dev CLI run as root, or a system-initiated launch), the app therefore
> inherits **root** inside the jail — a direct violation of this invariant. This
> is a bring-up shortcut, not the design. The fix: allocate a per-app uid
> (`portcullis_peer`, `APP_UID_BASE` = 50000), ensure a host passwd entry for
> `exec.jail_user`, register `uid → (user, app_id)`, and route the exec through
> jaild so its uid-range validation is the enforcement point. Until then, do not
> treat the jailed-desktop bring-up as evidence that the privilege boundary holds.

> ✅ **CLOSED 2026-09-13 — filesystem & device isolation now enforced for every
> jaild jail.** (Found 2026-06-24: every jail was created with `path = "/"`, so a
> jailed app saw the entire host filesystem and the full host `/dev` — `kmem`,
> `mem`, `pci` — with PID isolation only.)
>
> How it closed:
> - **Session apps** moved to real roots earlier (ostiarius's `spec()` and the
>   `session.d` manifests), with the standard rootfs nullfs mounts and a per-jail
>   devfs that jaild mounts at `<root>/dev`.
> - **The last `path = "/"` jails were the ten smoke manifests** in
>   `etc/services.d`. They now run on real roots with minimal read-only mounts.
> - **jaild refuses `path = "/"`** (`path.host_root`). The validator used to
>   special-case it in "because smoke tests use it"; the exception outlived its
>   reason, and keeping it open meant any manifest could walk back through it.
>   `portcullisd/tests/shipped_manifests.rs` checks every shipped manifest against
>   the shipped policy, so a regression fails `cargo test`.
> - **Mount destinations always resolve under the jail root.** A leading `/`
>   used to mean a HOST path at create time (runtime AttachMount already
>   re-rooted it). Invisible while every jail was on `/`, where the two coincide;
>   on a real root it put capability sockets and volumes on the host instead of in
>   the jail — `stoad`'s `mount_at = "/atrium-data"` would have — and it let an
>   allow-listed source be mounted over any host path.
>
> Not in this note's scope: the user-app launch path's per-app uid (the ⚠️ note
> above) and per-capability socket scoping.

- **App-to-app isolation.** A compromised app cannot read another
  app's files (no shared mount), cannot talk to services it
  didn't declare (socket not visible), cannot see other apps'
  processes (jail PID namespace).
- **App-to-host isolation.** Standard FreeBSD jail protections —
  no access to host filesystem outside declared mounts, no raw
  sockets, no kernel modules, no dev nodes outside the devfs
  ruleset. *(Enforced since 2026-09-13 — see the §9.1 note above.)*
- **Capability auditability.** The grant list is a human-readable
  text file. Users can revoke anytime by editing or via UI.
- **Manifest-tampering detection.** The grant record includes the
  manifest's content hash. Any change forces a re-prompt — apps
  can't silently expand their permissions across upgrades.

### 9.1a jaild's privilege drop kept root's supplementary groups — FIXED

**Found:** `ffi::drop_privileges` called `setgid` then `setuid` and **never `setgroups`**.
jaild runs as root, whose supplementary groups are `wheel`(0) and `operator`(5), and
`setgid`/`setuid` change the real and effective ids while leaving the supplementary list
exactly as it was. So every process jaild ever exec'd into a jail — every "dropped" service
at uid 1001 or 50090 — still carried wheel and operator. On FreeBSD `operator` owns the raw
disk devices and `wheel` gates `su` and a long tail of files. The uid and gid fields looked
dropped; the credential was not. Surfaced while mapping jaild for the one-shot lane, not by
any test — nothing in the tree ever looked at a child's groups.

**Proven live, not inferred**, by an A/B on the VM with two isolated jaild instances on
their own sockets (the installed daemon untouched), each exec'ing into a jail as 1001:1001:

| jaild | child's own verdict |
|---|---|
| self-check, **without** `setgroups` | `supplementary groups [0, 5] survived (want only 1001)` |
| self-check, **with** `setgroups` | drop passed; proceeds to `execve` |

**Fixed:** `setgroups(1, [gid])` first — it needs privilege, so it must precede `setuid`,
and the list `{gid}` is correct under both the historical semantics and FreeBSD 15+'s where
`setgroups` no longer touches the egid.

**And the drop now verifies itself, fatally.** `verify_dropped` checks real and effective
uid and gid, that no supplementary group other than the target survived, and that
`setuid(0)` fails afterwards. A failure exits the child rather than letting it run with
anything left over. The bug survived precisely because nothing looked at the result, so the
check is permanent defence rather than a one-time audit: a future edit that reorders the
calls, or a platform whose `setgroups` behaves differently, fails loudly at the first launch
instead of silently at the first compromise.

Deployed to the dev VM's installed jaild and re-verified through the real socket.

**Two harness defects found on the way.** `atrium-portcullisd-jclient` closed the procdesc
the instant it arrived, and because jaild pdforks without `PD_DAEMON` that *kills the child*
— so nothing about a child launched through it could ever be observed. It now honours
`ATRIUM_JCLIENT_HOLD=<secs>`. And the child's stderr goes to `/var/log/atrium/<name>.log`,
which is where the verdict above was read.

**Two of the gaps the same survey found are now fixed:**

- **`gid` is validated.** The policy schema had carried a *required* `[gid]` section —
  "mirrors uid table" — since it was written, and the validator never read it. A request
  could name gid 0, and after the `setgroups({gid})` fix that would have been the child's
  *only* group: wheel. The rule is now the uid rule: inside the user range or explicitly in
  `allowed_system_gids`. Every gid in use (1001, 1099, 50000, 50090–50094) passes; 0 and 5 are
  refused.
- **Every jaild tmpfs is sized.** tmpfs was mounted with no options at all, and an unsized
  tmpfs means *"all currently available memory"* (tmpfs(5)) — measured on the VM, an unsized
  mount reported exactly the free RAM at that instant (123 MiB, = 31,679 free pages). A
  per-jail rctl does not cover it: tmpfs pages belong to the filesystem, not to any process's
  RSS. Now an explicit size must be positive and within `mount_sources.max_tmpfs_mb`
  (default 256), and an absent size gets that ceiling rather than "unbounded" — so every
  existing caller, none of which passes a size, keeps working. Verified by A/B: the same
  unsized attach mounts at 256 MiB with the fix.

**Still open:** create-time mounts are never unmounted by `RemoveJail`; exec'd jails' state
records accumulate with no dedup; and rctl rules set through `SetRctl` are never removed.

### 9.1b Jails saw the host's entire /dev — unloaded devfs rulesets — FIXED

**Found (2026-09-22), while scoping the move of the one-shot lane onto jaild.** Four devfs
ruleset numbers were in use and **none of them was ever loaded into the kernel**:

| ruleset | used by | defined where |
|---|---|---|
| 99 | every portcullis app launch and every one-shot worker | nowhere — "Phase 4 manages allocation" |
| 100 | `atrium-session` user session jails | nowhere — "picked above 99" |
| 20 | memoryd, memfed (via jaild) | a file saying "append this to /etc/devfs.rules by hand" |
| 21 | frescod (via jaild) | the same, a second file |

**A ruleset the kernel has not loaded does not fail the mount.** `devfs_ruleset_use` creates
it *empty* and takes a reference, so the mount succeeds and hides nothing. Measured on the VM:
a devfs mounted with ruleset 99 showed **64 host nodes** — `mem`, `kmem`, `bpf`, `pci`,
`devctl`, `klog` and the raw disks — the same inside a real `jail(8)` with those parameters,
and **a root process in that jail read the host's disk** (`dd if=/dev/vtbd2`, the scratch
disk, read-only). Unprivileged workers were held back only by node permissions (disks are
`root:operator 0640` — the group §9.1a's missing `setgroups` had been leaking).

A second defect hid behind the first: **capability device grants were computed and never
applied.** `audio`, `usb-hid`, `camera`, `graphics` record devfs unhide actions, and
`render_devfs_rules` rendered them, and nothing ever loaded the result. Apps had their devices
only because nothing was hidden at all.

**Fixed, in three layers:**

- **Fail closed at every mount site.** jaild (`devfs_ruleset.not_loaded`) and the four
  `jail -c` paths in portcullis (`portcullis_mounts::ensure_devfs_isolation`: daemon launch,
  CLI launch, one-shot, atrium-session) refuse a jail whose ruleset has **no rules** — not
  "is not listed", because once any jail has mounted an unloaded number it *is* listed. They
  also refuse `mount.devfs` with no ruleset or 0. Forgetting the rules file is now a jail that
  does not start and says why, instead of a silent exposure.
- **One rules file, installed and loaded.** `etc/atrium.devfs.rules` defines 20
  (`atrium_governor`), 21 (`atrium_gpu`) and 22 (`atrium_app`: hide all, unhide the basics and
  the pty/fd set). `scripts/bootstrap-atrium.sh` installs it, adds it to `devfs_rulesets` so
  rc.d/devfs loads it on every boot, loads it immediately and checks all three loaded.
  Apps and one-shots use 22 (`portcullis_jail::APP_DEVFS_RULESET`); session jails use
  FreeBSD's standard 4 (`devfsrules_jail`), matching ostiarius.
- **Capability grants applied per jail.** `build` emits `exec.prestart` =
  `devfs -m <root>/dev rule apply <rule> && …` for each grant. `rule apply` changes one mount
  without adding to any ruleset, so no per-app ruleset numbers are allocated; `prestart`
  because jail(8) mounts devfs before it and creates the jail after it, so the grant is in
  place before anything runs; `&&` so a grant that fails fails the launch.

**Verified on the VM, both directions:**

| check | before rules loaded | after |
|---|---|---|
| jaild CreateJail, ruleset 21 | refused `devfs_ruleset.not_loaded`, nothing left | created; `/dev` = 4 nodes; vtbd0/kmem/bpf hidden |
| jaild CreateJail, ruleset 22 | — | created; 8 nodes; vtbd0/kmem/bpf hidden |
| `portcullis exec` one-shot | refused with the reason; 0 mounts, 0 dirs left | live worker's `/dev`: `fd null random stderr stdin stdout urandom zero` |
| Navigator corpus E2E | refused | 98 sessions / 259 navs / 259 rewinds / 0 failures, 59.2 s (was 59.3 s) |
| per-mount grant | — | `rule apply path 'bpf*' unhide` on a ruleset-22 mount reveals bpf there only; ruleset 22 still 3 rules |

**A teardown leak the refusal exposed.** The first refused run left the read-only tree and the
tmpfs upper layer mounted. `portcullis_mounts::parse_mounts` de-duplicated mount *paths*, but a
one-shot root is two mounts at one path (the tree and the union over it); a pass that removed
the union therefore counted as no progress and `converge` stopped. The normal exit path
happened to avoid it, so no earlier failure had ever exercised it. Layers are now counted
individually, and the one-shot teardown removes the (empty) jail root on every exit.

### 9.1c Host identity — synthetic, per app, never the real machine's

**Found (2026-09-22), prompted by "what does a licence manager see?".** Measured inside a
jail made the way jaild made them:

| identifier | host | jail (before) |
|---|---|---|
| hostname | `atrium-devroot` | empty |
| `kern.hostid` (`gethostid(3)`) | `1333810599` | `0` |
| `kern.hostuuid` | `94544ab3-…` | all zeros |
| Ethernet MAC | `52:54:00:12:34:56` | **the host's, visible** (non-vnet jail) |

Two failures at once: host-keyed software (FlexLM's `lmhostid` and kin) got a hostid of 0 —
the same on every machine, so either refused or no longer node-locked — while the one thing a
jail *could* read, the MAC, is a stable machine-wide fingerprint every app can correlate on.

**Decided (user): every app gets a SYNTHETIC identity, and no capability exposes the real
one.** `portcullis-identity` derives it as `HMAC-SHA256(machine secret, app id)` with a
domain label: **stable** (same app, same machine → same values across launches and reboots,
so a licence activated against it keeps working), **machine-bound** (the secret is per
machine), **per app** (two apps cannot compare notes), and **unrelated to real hardware**.
hostid is never 0; the UUID is RFC 9562 version 8. The secret is 32 random bytes at
`/var/db/atrium/host-identity.key`, root, 0600, created on first use; a file of the wrong
size, owner or mode is refused, not repaired — repairing would silently re-key every app and
void every licence bound to it.

**Wired so it cannot be forgotten.** `BuildOpts::host_identity` is REQUIRED — a launch path
without it does not compile — and emits `host.hostid`/`host.hostuuid` beside
`host.hostname` (= app id). jaild's `CreateJailRequest` carries `hostname`/`hostid`/`hostuuid`
(shape-validated: DNS-style name, hostid ≠ 0, lowercase non-zero UUID; `host.hostid` passed
as the kernel's `unsigned long`); jaild does no crypto, and its hostname defaults to the jail
name so no jail is left nameless. The one-shot lane uses the app id — not the instance tag —
so every worker of an app is the same "machine" to it. Session jails derive from
`session:<user>`; a real session `create` refuses without the real identity rather than use
the render-only placeholder.

**Verified in the VM:** one-shot jails (jaild lane) and a `portcullis launch` (jail(8) lane)
of the same app report the same `org.atrium.navigator.worker` / `2469032643` /
`6a79b4fb-…-8617-…`, identical across instances and nothing in common with the host's; inside
a jail `hostname`, `kern.hostid` and `kern.hostuuid` return the jail's values; corpus E2E
unchanged (98/259/259/0).

**Found on the way:** the CLI's `launch` created none of the mountpoints jail(8) needs
(`dev`, the run user's home, capability mountpoints — the daemon's launch made all three) and
failed at `mount.devfs: …/dev: No such file or directory` before the jail existed. Fixed.

**The MAC — hidden for apps without network (2026-09-22).** A non-vnet jail lists the host's
interfaces and their real MACs, so "no network" is now an own, EMPTY vnet (`vnet=new`,
nothing moved in): the jail's stack holds only its own `lo0`. jail(8) lane:
`NetworkCap::None` → `vnet = new` (vnet refuses any ip4/ip6 setting — "vnet jails cannot have
IP address restrictions"); jaild lane: new `NetworkConfig::Isolated`, used by every one-shot
(`Disable` stays for Atrium's own services). Verified: one-shot and launched jails show
interfaces `lo0` only, 0 `ether` lines. Cost: ~47 ms per create+remove against ~1 ms (measured,
20 each); invisible at corpus scale (98 docs: 58.3 s vs 59.2 s).

**Found on the way — the `loopback` capability never worked.** It set `ip4.addr`/`ip6.addr` on a
vnet jail, which the kernel refuses; its unit test pinned exactly that recipe and passed. Now:
`vnet = new` plus `exec.created` = `ifconfig -j <jail> lo0 inet 127.0.0.1/8 up …` (on the host,
after creation, before the app starts). Verified: a signed loopback app launches with its own
`lo0` UP on 127.0.0.1.

**Apps WITH network (2026-09-22):** decided and built per network.md §0 — an own stack with a
point-to-point epair carrying a MAC derived like the hostid (`HostIdentity::mac`), NAT out,
pf blocking app→host and app→app. Done for one-shots (verified in the VM: derived MAC,
internet allowed, host blocked) and for apps launched through the jail(8) lane (same MAC per
app on both lanes; network.md §0 step 3). `vnet = inherit` is gone: no jailed app reads the
host's MAC or hostid any more.

### 9.2 Out of scope

- **App-as-trojan.** A user-granted app can use its capabilities
  maliciously within its grant. Atrium can warn ("this app wants
  network and filesystem — could exfiltrate") but not prevent.
  Mitigation: minimize default-grant capabilities; make scary
  ones (network=full + filesystem) very explicit in the prompt.
- **Side-channel leaks.** Two apps sharing the clipboard service
  can communicate covertly (one paste, the other reads). Same
  shape on every desktop OS. Per-app clipboard scoping in the
  daemon is a mitigation, not a guarantee.
- **Compromised system services.** A compromised Fresco gives
  the attacker access to all rendered content (every app's pixels).
  This is intrinsic to having a centralised compositor. Mitigation
  in the privsep architecture (§0.5): Fresco runs in its own jail
  with a tiny mount set + no network + no filesystem write outside
  its socket directory. A Fresco RCE doesn't escape that jail. The
  attacker still has Fresco's privileges *within* the jail — keys,
  pixels, GPU device — but no fs/network access beyond.

### 9.3 Trust hierarchy

| Component | TCB rank | Compromise impact |
|-----------|----------|-------------------|
| FreeBSD kernel | 0 (fully trusted) | game over |
| jaild | 1 (audited, ~500 LoC, no business logic) | can create arbitrary jails permitted by policy file; cannot escape its own jail |
| jaild policy file (`/etc/atrium/jaild.policy.toml`) | 1 (root of policy trust) | bad policy → broad jails. Change-control + future signing. |
| portcullisd | 2 (Capsicum'd, larger interpreter) | can ask jaild for things in policy; cannot escape jaild policy |
| atrium-authd (deferred, v1.5) | 2 (auth helper) | can verify credentials; no exec or jail authority |
| frescod | 3 (jailed system service) | sees all rendered content; no fs/network |
| vestibulum | 3 (jailed pre-auth screen) | can claim arbitrary user authenticated (worst case in v1; tightened by atrium-authd in v1.5) |
| atrium-devevents | 3 (jailed input reader) | sees all keyboard/mouse input pre-routing |
| user supervisor | 4 (jailed, uid=N) | bounded by user-N's grants |
| user app | 5 (jailed, per-manifest caps) | bounded by capability set |

## 10. Implementation phases

Order matches risk (smallest blast radius first). Phases 0a/0b
were added in the 2026-05-07 privsep revision (§0.5); the existing
phases 1-5 keep their numbering and shape.

**Phase 0a — jaild policy schema (landed 2026-05-07, commit 8842358).**
- `jaild-policy` crate: serde-typed schema for
  `/etc/atrium/jaild.policy.toml`, parser, schema-version check,
  shipped sample at `etc/jaild.policy.toml`.
- Two unit tests green: parses sample, refuses bad version.
- ~½ week (done).

**Phase 0b — jaild daemon.**
- New `jaild` crate. Reads policy file at startup. Listens on
  `/var/run/atrium/jaild.sock`. Per-request: validate against
  policy → `pdfork` → child does `jail_set` + `execve` → parent
  returns jid + procdesc fd via `SCM_RIGHTS`.
- `jail_remove` on request from portcullisd or on procdesc EOF
  cleanup.
- Persistent state file at `/var/run/atrium/jaild.state.toml`
  for crash recovery (per `docs/spec/login-handoff.md` Phase 5).
- Smoke validation pre-existed: `scratch/jail-smoke/jaild-privsep.c`.
- Real implementation: ~1 week.

**Phase 1 — schema + parser + validator.**
- `portcullis-toml` crate: serde-deserialize `atrium.toml`,
  validation rules from §3.3, golden-file tests for accept and
  reject cases.
- CLI: `portcullis validate <atrium.toml>` for app authors and
  `tessera-import` integration.
- ~1 week.

**Phase 2 — jail builder (no policy, no prompts).**
- `portcullis-jail` crate: capability → jail.conf translation
  per §5. Round-trip (parse → translate → parse) tests for every
  capability class.
- CLI: `portcullis launch --no-prompt <tree>` runs an app with
  ALL declared capabilities granted (dev mode).
- Integration: launch aqueduct-echo-server in a jail and have
  aqueduct-echo-client (also in a jail) talk to it through
  the nullfs-mounted socket. Validates the IPC capability path.
- ~1 week.

**Phase 3 — overlay + rootfs union mounts.**
- Read-only Tessera rootfs + Tessera overlay + nullfs unionfs.
- Test: launch the same app from two parallel jails, verify
  isolation of overlay state but sharing of rootfs.
- ~1 week.

**Phase 4 — portcullisd + capability policy.**
- Long-running daemon. aqueduct service for portcullis-cli
  to query/grant capabilities.
- Policy file format + persistence at `/var/db/atrium/<user>/
  policy.toml`.
- Implements the lifecycle in §6 end-to-end.
- **Privsep integration (per §0.5):** portcullisd opens its
  jaild socket fd at startup (env var `ATRIUM_JAILD_FD`),
  reads its initial state, then `cap_enter()`. From that
  point all jail-creation goes through jaild. portcullisd
  holds procdesc fds (received via `SCM_RIGHTS` from jaild
  alongside each jid) and uses `EVFILT_PROCDESC` on
  `NOTE_EXIT` for lifecycle.
- ~1 week.

  *Step 1 (landed):* `portcullis-policy` crate — Policy/Grant
  data model, atomic load/save, manifest-hash tamper detection,
  capability-delta computation. CLI subcommands `policy show`,
  `policy diff`, `policy grant`, `policy revoke` for managing
  the policy file by hand.

  *Step 2 (landed):* Default `portcullis launch` mode now
  consults the per-user policy file. Refuses if the manifest
  asks for capabilities the user hasn't granted, with a
  diff-style message and a hint to `policy grant`. `--no-prompt`
  becomes the explicit dev-mode bypass; `--dry-run` skips the
  check (no execution → no policy needed).

  *Step 3 (landed):* `portcullis-ipc` crate (newline-delimited
  JSON over Unix-domain socket; Hello/Ping/Authorize/Grant/
  Revoke/Reload ops + ProtoMismatch handshake) and the
  `portcullisd` binary (one thread per connection, in-memory
  policy behind a Mutex, mode-0600 socket at
  /var/run/portcullisd.sock). Daemon delegates to
  portcullis-policy so the wire surface and the CLI's direct
  file path stay semantically identical.

  *Step 4 (landed):* CLI now tries portcullisd first for every
  policy operation (launch authorize, `policy grant`, `policy
  revoke`); on socket-not-present it falls back to direct
  policy.toml access. New `portcullis daemon ping/reload`
  subcommands. `$PORTCULLIS_SOCKET` env override for non-root
  development.

  Phase 4 is now complete: a long-running daemon is the
  canonical policy writer when present, and the CLI degrades
  gracefully without it. Phase 5 adds the actual interactive
  prompt UI on top of `Authorize → NeedsApproval` replies.

**Phase 4.4 — daemon owns launch + session jail.**
- The whole "user lands in an unjailed shell after login" hole
  the original spec papered over. We close it by:
  (a) moving the privileged side of launch (mount, jail -c,
      teardown) from the CLI into portcullisd, so the launching
      client doesn't need root.
  (b) adding a per-user *session jail* with the host base
      mounted read-only, /apps as a read-only view of installed
      apps, and the portcullisd socket bind-mounted in.
  (c) using `zsh` as the in-jail login shell with a curated
      /etc/zshrc (sensible prompt + tab-completion for
      `launch <app-id>`). The jail is the security boundary;
      the shell is just the shell.
- Escape hatches during dev: single-user mode (always),
  plus a `dev` user with `/bin/sh` as login shell that gets
  removed for production.
- ~1 week.

  *Step 1 (landed):* `Request::Launch{app_id, bypass_policy}`
  added to the IPC; daemon-side `launch.rs` carries the
  mount/jail-c/teardown logic; CLI `launch <id>` forwards to
  the daemon when present and falls back to in-process launch
  otherwise. Stdio inherits the daemon (app output → daemon
  log) for now — SCM_RIGHTS pty passing in step 2.

  *Step 2 (landed):* SCM_RIGHTS fd handoff. Wire dance:
    CLI → Launch{app_id, bypass_policy}
    daemon ← parses, runs policy gate, replies ReadyForFds
    CLI → sendmsg([1 byte 'F'], cmsg=SCM_RIGHTS[stdin,stdout,stderr])
    daemon ← recvmsg picks up the 3 OwnedFds, runs jail(8) with
            them as 0/1/2, returns LaunchExit{code}
  The ReadyForFds round-trip drains the daemon's BufReader so
  its plain-read can't silently swallow the cmsg's data byte.
  fdpass.rs uses libc directly (~80 lines of unsafe vs.
  pulling in nix/passfd as a dep). Verified with a 3-fd
  pipe-pair round-trip test.

  *Step 3 (landed):* `atrium-session` crate. Composes a per-user
  session jail under /var/lib/atrium/sessions/<user>/ with
  selective host base bind-mounts (/bin, /sbin, /lib, /libexec,
  /usr — NOT /etc, /var, /home, /root), a writable per-user
  overlay over the union, /apps as a read-only view of installed
  apps, and /atrium/sockets/ bind-mounted so the in-jail CLI
  reaches portcullisd.

  Subcommands: `render`, `create`, `destroy`. `persist=true`
  jail (lifecycle decoupled from the shell process); login(8)
  in step 4 will `jexec` zsh into the running jail. Curated
  /etc tree (passwd/group/zshrc/login.conf) deferred to the
  login-wiring step.

  *Step 4 (landed):* Login wiring via wrapper-shell route
  (PAM rejected — see Phase 4 design notes for the cost
  analysis: language-policy collision, system-wide blast
  radius, slower test loop, config sprawl). The wrapper
  ships at atrium-session/install/atrium-login as a 30-line
  POSIX shell script: waits up to 5s for the portcullisd
  socket, then execs `atrium-session enter $USER`. New
  `atrium-session enter` subcommand is idempotent (no-op if
  the jail is already running) and uses jexec -l -U to drop
  to the user. Curated /etc tree composer (passwd, group,
  shells, zshrc with `_atrium_launch` completion + `apps`
  alias + welcome banner) lands per-session at
  /var/lib/atrium/sessions/<user>/etc/, bind-mounted RO at
  jail/etc.

  *Step 5 (landed):* `portcullis link-apps` walks APPS_DIR
  and drops a 3-line wrapper at /var/lib/atrium/apps/<id>/<id>
  per installed app (mode 0755). Inside the session jail,
  `./apps/<id>/<id>` execs `portcullis launch <id>`. tessera-
  import will call this on every install in a future commit;
  for now run it by hand.

  Phase 4.4 is now complete: a logged-in user lands in a
  jailed shell with no path to the unjailed host, can list
  installed apps via `apps`, and launches them through
  portcullisd which mounts/jail-c's the per-app jail with
  the user's tty handed over via SCM_RIGHTS.

**Phase 4.5 — first-run setup phase (landed).**
  - Sentinel: `<overlay>/.atrium-firstrun-done`. Check on every
    launch; if absent and `[setup]` exists, run setup; on success
    write sentinel.
  - Implementation note: spec's "use jail.conf exec.created"
    doesn't actually work, because `network = "full"` during
    setup-only requires `vnet=inherit` for the setup phase but
    not the runtime — and vnet is fixed for a jail's lifetime.
    The implementation runs setup and runtime as **two separate
    jail-c invocations** sharing the same overlay mount. First
    jail = "<id>_setup" with merged caps + entry =
    `setup.command`. Second jail = the runtime jail as before.
    Overlay mount-once / unmount-once spans both.
  - Capability merge: new `merge_capabilities(base, override)`
    in portcullis-toml. Override is bidirectional — setup may
    add network OR drop network relative to runtime. Lists
    (filesystem, fonts.paths) replace wholesale.
  - Setup failure (non-zero exit) aborts the launch and leaves
    the sentinel absent so the next launch retries.
  - `portcullis reinstall <app-id>` — wipe the sentinel to force
    re-setup. Refuses if jail running.
  - Stdio: setup phase inherits the daemon (output → daemon
    log) for now. A follow-up step can pipe setup output through
    the same SCM_RIGHTS pty as the runtime app so the user sees
    "running first-time setup..." on their terminal.

  Out of scope for this commit (deferred):
  - `setup.timeout` enforcement (spec mentions "120s"; today
    it's parsed but not honored).
  - Shared `/var/cache/atrium/pkg/` fetch cache mount with
    `fetch-cache = "pkg"` capability.

**Phase 4.5 — first-run setup phase.**
- Per-app overlay sentinel (`.atrium-firstrun-done`) detection.
- jail.conf `exec.created` invocation when sentinel absent
  + `[setup]` is present.
- Setup-phase capability application (network etc., dropped
  after).
- Optional shared fetch cache mount at `/var/cache/atrium/pkg/`
  for apps that opt in via `fetch-cache = "pkg"` capability.
- `portcullis reinstall <app>` CLI to wipe sentinel/overlay.
- ~½ wk.

**Phase 5 — capability prompt UI (CLI tty version landed).**
- IPC: new `Response::LaunchNeedsApproval { delta }` distinct
  from `LaunchFailed` — daemon emits it when the policy gate
  refuses, instead of conflating policy refusal with manifest
  errors.
- CLI prompt on the controlling tty:
      Allow? [o]nce, [a]lways, [d]eny
  - **Allow once** → re-issue Launch with bypass_policy=true
    (nothing persisted to policy.toml).
  - **Allow always** → persist a grant for the manifest's full
    capability set (daemon-first, file-fallback) then re-issue.
  - **Deny / Ctrl-D / empty input** → exit 1.
- TTY detection via libc::isatty on stdin AND stderr (where the
  prompt text lives). Non-tty contexts (scripts, cron) fall
  back to the pre-Phase-5 behaviour: print the delta + a hint
  to run `policy grant` or `--no-prompt`, exit 1.
- Same prompt logic wired into both code paths: the daemon-
  forward case (the common one) and the in-CLI fallback for
  when portcullisd isn't running.
- GUI version (Forum-rendered) is D3 work — slot in by adding
  a parallel `Response::LaunchNeedsApproval` consumer that
  routes the prompt through the desktop instead of stdin.

D2.5 is "complete" when an end-to-end demo works:
- atrium-edit-socket installed via `tessera-import` into a
  managed Tessera location, with `[packages.freebsd]` declaring
  any pkg deps.
- Pool ensures declared packages are present (single install per
  package globally).
- `portcullis launch org.atrium.edit` runs it in a jail with
  exactly the capabilities + the pool-mounted package files.
- Removes capabilities or packages → app can't access them →
  fails cleanly.
- Two parallel instances → isolated overlays, shared rootfs,
  shared pool.
- Two different apps both depending on `openssl` → one pool entry,
  zero per-app duplicate disk.

## 11. Open questions

### 11.0 Resolved 2026-05-07

- **Where does jail creation live?** Resolved by §0.5: a privsep
  daemon (jaild) is the sole `jail_set` caller; portcullisd is
  Capsicum-confined and asks jaild via socket. Validated by
  `scratch/jail-smoke/`.
- **GPU-capability gate:** spec'd in `docs/spec/gpu-isolation.md`.
  Two-tier: `render-only` (frescod-mediated, always grantable) vs
  `gpu-direct` (requires kernel-driver attestation in
  `jaild.policy.toml` `[gpu_drivers.attested.<name>]`).
- **Login privilege handoff:** spec'd in
  `docs/spec/login-handoff.md`. `pdfork` + `EVFILT_PROCDESC` is
  the spine; vestibulum sends portcullisd a nonce-bearing
  `session_start`, portcullisd asks jaild to create the user
  supervisor jail with `setuid(N)` + execve.
- **Compromised system services:** §9.3 trust hierarchy makes the
  blast radius explicit. Frescod / vestibulum / atrium-devevents
  all run jailed; a compromise gets the privileges of the service
  but no fs/network escape.

### 11.1 Still open

- **App identity for jails:** `app.id` directly as the jail name
  vs. derived UUID? Direct id is human-readable in `jls` output;
  UUID handles the case where two apps claim the same id (refuse
  on import, probably).
- **Per-launch vs per-app jails:** if I launch `atrium-edit`
  twice, is that one jail with two processes, or two jails?
  Two jails feels right for security (one crash doesn't take
  the other down) but doubles the per-instance overhead. Per
  app.toml `[supervision].instances = "single" | "multi"`.
- **`network = "loopback"` semantics:** shared loopback (apps
  can find each other on 127.0.0.1) vs per-jail loopback alias
  (each jail gets `127.0.0.<n>/8`)? Latter is more isolated;
  former is more compatible with apps that bind to fixed ports.
- **Hot-reload of capabilities:** can a user revoke a capability
  while an app is running? Probably "it takes effect on next
  launch" (jail.conf mounts are set at jail-create time;
  changing them mid-jail is hairy). Document the limitation.
- **Default grants:** should `graphics = "fresco"` be auto-
  granted (no prompt)? Probably yes — every app needs a window
  or it's a daemon, and daemons go through a different path.
  Same for `notify`. The non-trivial ones (network, filesystem)
  always prompt.
- **Service installation:** how does a *system service* (clipboard
  daemon, notify daemon, broker) get installed? They're not user
  apps. Probably via a privileged installer step that drops
  their socket into `/atrium/sockets/` with appropriate ownership;
  Portcullis then nullfs-mounts.
- **Multi-user:** Atrium currently assumes single-user-session.
  Multi-user would mean per-user `/atrium/sockets/` namespaces
  (e.g. `/atrium/sockets/<uid>/clipboard.sock`). Defer; current
  design mounts the singleton path.
- **Setup script timeouts:** what's a reasonable default cap?
  120 s feels right for "pkg install a few things" but apps that
  download multi-GB ML models need more. Manifest declares its
  own; Portcullis enforces the cap. Document expectation that
  long-running setup should show progress to the user (probably
  via the prompt UI showing "still installing…").
- **Setup failures:** if setup script exits non-zero, leave
  sentinel absent and let next launch retry? Or fail the launch
  permanently and require user intervention? Probably retry-on-
  next-launch up to N times, then surface as a UI error.
- **Setup script auditability:** users can inspect the script
  before granting setup capabilities. UI should make this easy
  ("Show setup script") so users aren't approving a black box.

### 11.2 Tracked but deferred

- **`atrium-authd` privsep helper.** Per `login-handoff.md`,
  vestibulum currently does `crypt(3)` directly. v1.5 hardening
  splits this into a small auth-only daemon (OpenSSH-shaped),
  bringing vestibulum's TCB down to "shows UI; forwards
  credentials to authd; receives one-shot token; forwards token
  to portcullisd."
- **Policy-file signing.** `jaild.policy.toml` is the runtime
  root of trust. Future: minisign-style detached signature so
  jaild can verify the file is the operator-approved one before
  loading. Defer; relies on key management story we don't have
  yet.
- **`test_gpu_isolation` regression.** The parametric kernel
  test in `gpu-isolation.md` §Validation is spec-only; landing
  it in `atrium-kmod/` is a prereq for any future driver
  switching its attestation entry to `production`. Required for
  D5; optional for D0+V7 (already attested via the layered chain).
- **Per-context IOMMU enforcement on D5+ native GPU drivers.**
  Each driver port needs a section under
  `atrium-kmod/<port>/ISOLATION.md` mapping invariants I1–I5 to
  HW mechanisms. Required before any `gpu-direct` grant for that
  driver.
- **Setup-phase capability prompts:** the prompt UI shows the
  setup-vs-runtime split (see §3.4 example). Capabilities only
  elevated during setup are still surfaced — network during
  setup is still network access, the user should know. Default-
  grant rules apply to setup the same way (graphics + notify
  silent; everything else prompts).
- **Security updates:** how do CVEs get patched? Up to the app —
  it might re-check on launch and re-pkg-install if outdated.
  Atrium might surface a "no app has been re-installed in 90
  days" warning via the prompt UI. Operations-layer policy.
