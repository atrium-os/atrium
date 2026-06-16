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
- Per-jail rootfs trees / overlay+unionfs. Smoke manifests use `path = "/"` as a transitional stand-in; D5 atrium-rootfs lands real per-jail trees.
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

Each overlay is provisioned inside its own quota domain
(tessera-quotas.md), which simultaneously sets the dedup-domain
boundary and gives the jail quota-scoped `statfs`
(tessera-quotas.md §3.6) — a jail never sees the pool-physical
free-space counter. atrium-volumes' tessera plugin sets
`dedup_policy` at domain creation.

### 4.2 Per-jail layout (split across three trees)

Per-app state is split across three sibling trees under
`/var/lib/atrium/`, all subtrees of the same Tessera volume:

```
/var/lib/atrium/
├── apps/<app.id>/             ← lower layer: dedup'd app tree (Tessera)
│   ├── bin/atrium-edit            (read-only at launch; never mutated
│   ├── lib/...                     by the jail — preserves cross-jail
│   ├── share/...                   CAS dedup of binaries + libs)
│   └── atrium.toml
├── overlays/<app.id>/         ← upper layer: per-app writable (Tessera)
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

1. Mounts `apps/<id>/` read-only via nullfs at `jails/<id>/`.
2. Mounts `overlays/<id>/` writable via unionfs over the same
   `jails/<id>/`. Writes inside the jail land in the overlay;
   reads see the union.
3. Sets `jail.path = /var/lib/atrium/jails/<id>/` and runs
   `jail -c`.

On jail exit (or `jail -r`): tear down in reverse order — devfs,
unionfs, nullfs.

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

### 9.1 In scope

- **App-to-app isolation.** A compromised app cannot read another
  app's files (no shared mount), cannot talk to services it
  didn't declare (socket not visible), cannot see other apps'
  processes (jail PID namespace).
- **App-to-host isolation.** Standard FreeBSD jail protections —
  no access to host filesystem outside declared mounts, no raw
  sockets, no kernel modules, no dev nodes outside the devfs
  ruleset.
- **Capability auditability.** The grant list is a human-readable
  text file. Users can revoke anytime by editing or via UI.
- **Manifest-tampering detection.** The grant record includes the
  manifest's content hash. Any change forces a re-prompt — apps
  can't silently expand their permissions across upgrades.

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
