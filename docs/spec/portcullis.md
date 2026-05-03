# Portcullis — jail launcher + capability manifest

Status: design (D2.5).
Last updated: 2026-05-03.

The piece of Atrium that turns "an app" into "a running, isolated,
capability-scoped process." Portcullis reads each app's
`atrium.toml`, builds a jail with exactly the capabilities the
manifest declares, and execs the app inside. Forum (D3, the dock /
launcher) calls Portcullis; Vestibulum (D2, login) launches the
session supervisor that does likewise.

Portcullis is to Atrium what `systemd-nspawn` is to Linux + the
permissions-model of mobile OSes — but built natively on FreeBSD
jails, devfs.rules, nullfs, rctl, and the Atrium substrate
(Tessera CAS-FS, atrium-rpc).

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
  jail trees with cross-jail dedup; atrium-rpc capability
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
network     = true                # transient network during setup only
timeout     = "120s"

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
apps) require explicit policy approval beyond user prompt — only
granted to system services or apps with manual admin override.

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
network = true                    # transient network during setup
timeout = "120s"                  # fail if it hangs
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

Setup capabilities are *additive* over runtime capabilities. A
user prompt at first install distinguishes the two:

```
"Atrium Edit" wants:

  At setup (one-time, first launch):
      ✓ Network access
      ✓ Up to 120 seconds runtime
  Always (every launch):
      ✓ Display (Fresco)
      ✓ Clipboard
      ✓ Notifications
      ✓ Read/write your Documents folder

[Allow once]   [Allow always]   [Deny]
```

After setup completes successfully, the network capability is
dropped — the runtime jail has no network unless `[capabilities].
network` says otherwise.

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
file (e.g., the same `libssl.so.3`), Tessera's pack registry
finds the existing blob and dedupes — zero extra disk used.

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

## 4. Jail filesystem layout

### 4.1 Single shared Tessera volume

**All Atrium jails are subtrees of one underlying Tessera volume,
not separate per-jail volumes.** This is the load-bearing
architectural choice that makes cross-jail dedup work:

- Tessera's CAS layer (pack registry, blob hashes, dedup) is
  per-volume.
- Two subtrees of the same volume → blobs are shared via the same
  pack registry → cross-jail dedup is automatic and total.
- Two separate volumes → blobs are independently stored → zero
  cross-volume dedup.

The shared volume lives at `/var/lib/atrium/store.tessera`
(or wherever the host installer puts it). All jails' rootfs +
overlay + (per-app pkg-installed files) are subtrees inside
this single volume.

### 4.2 Per-jail subtree

Per-app jail at `/var/lib/atrium/jails/<app.id>/` (a subtree of
the shared Tessera volume, NOT a separate Tessera image):

```
/var/lib/atrium/jails/org.atrium.edit/
├── rootfs/                    ← lower layer: dedup'd app tree (Tessera)
│   ├── bin/atrium-edit
│   ├── lib/...
│   ├── share/...
│   └── atrium.toml
├── overlay/                   ← upper layer: per-app writable (Tessera)
│   ├── home/                  ← what the app sees as $HOME inside the jail
│   ├── tmp/                   ← scratch (cleared on launch? configurable)
│   └── state/                 ← persisted app state (settings, cache)
└── runtime.conf               ← generated jail.conf section
```

The jail's `path` is a unionfs (or nullfs+overlay) of `rootfs`
(read-only Tessera-mounted) over `overlay` (read-write Tessera-
backed). Rootfs is shared across all instances of the same app
(content-addressed → one copy on disk regardless of how many
times "installed"). Overlay is per-(app, user-session).

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
| App A uninstalled (`portcullis remove`) | Already stopped | Deleted (subtree removed from Tessera) | Decrement; zero-ref blobs become GC-eligible |
| App B uninstalled later | — | Deleted | Refs reach 0 for blobs only B held → GC eventually reclaims |

The key invariant: **Tessera GC only touches blobs whose refcount
reaches zero.** A blob still referenced by ANY live inode (in any
subtree on the volume) cannot be reclaimed. So:

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

[setup] command = "scripts/firstrun.sh", network = true
  →  on first launch (no .atrium-firstrun-done sentinel):
       jail.conf adds setup-phase capabilities (network etc.)
       exec.created = "scripts/firstrun.sh"
       on success: write sentinel; remove network capability
       from runtime jail.conf for this app's subsequent launches
  →  on subsequent launches: setup is skipped; only runtime
     capabilities apply
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
│    - enters the jail; runs exec.start (= app.entry)        │
│    - returns the jail's pid                                │
└─────────────────────────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────────────────────────┐
│ 8. portcullisd supervises                                  │
│    - waitpid(jail.pid)                                     │
│    - on exit: per supervision policy (do nothing, restart, │
│      restart-on-crash, log the exit code)                  │
│    - on app-id reuse mid-session: ensure single instance   │
│      OR allow per-launch new jails (per atrium.toml)       │
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
5. If non-empty: send a prompt message via atrium-rpc to a UI service
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

### 8.2 atrium-rpc

- The capability boundary IS the substrate atrium-rpc was
  designed for. Each `<service> = true` line in the manifest
  becomes one nullfs mount of one socket. Apps without the
  capability literally cannot see the socket — atrium-rpc's
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
  the attacker access to all rendered content. Standard. Service
  hardening + capsicum are the answer; out of Portcullis's
  scope.

## 10. Implementation phases

Order matches risk (smallest blast radius first):

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
- Integration: launch atrium-rpc-echo-server in a jail and have
  atrium-rpc-echo-client (also in a jail) talk to it through
  the nullfs-mounted socket. Validates the IPC capability path.
- ~1 week.

**Phase 3 — overlay + rootfs union mounts.**
- Read-only Tessera rootfs + Tessera overlay + nullfs unionfs.
- Test: launch the same app from two parallel jails, verify
  isolation of overlay state but sharing of rootfs.
- ~1 week.

**Phase 4 — portcullisd + capability policy.**
- Long-running daemon. atrium-rpc service for portcullis-cli
  to query/grant capabilities.
- Policy file format + persistence at `/var/db/atrium/<user>/
  policy.toml`.
- Implements the lifecycle in §6 end-to-end.
- ~1 week.

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

**Phase 5 — capability prompt UI.**
- A minimal `atrium-prompt` service (or wired into Forum once
  D3 lands) that renders the prompt text + buttons.
- Connects portcullisd's "I need user input" to a real GUI.
- Until Forum exists, a CLI fallback: `tty` prompt for
  development, headless mode for testing.
- ~half-week.

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
- **Setup-phase capability prompts:** are setup capabilities
  always prompted (even if runtime caps are auto-granted)? Yes
  — network access during setup is still network access. Always
  visible in the prompt.
- **Security updates:** how do CVEs get patched? Up to the app —
  it might re-check on launch and re-pkg-install if outdated.
  Atrium might surface a "no app has been re-installed in 90
  days" warning via the prompt UI. Operations-layer policy.
