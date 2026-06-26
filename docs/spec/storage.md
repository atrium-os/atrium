# Atrium storage architecture

**Status:** spec + implementation, 2026-05-08
**Owner:** D2.5 Portcullis + atrium-volumes (new) + jaild

> **Implementation note (2026-05-08):** §§ 1–11 are implemented and
> VM-verified as of this date. §12 (operator install flow) and §13
> (interaction with atrium-pkg) are spec-only. §3 dropped the `cas`
> volume kind (every persistent volume on Tessera is content-addressed
> already; raw CAS-API access would be a separate primitive if a real
> consumer ever appears). See `ROADMAP.md` §D2.5 for per-phase status.

This doc records the architectural decisions for how Atrium-jailed
services get persistent and ephemeral filesystem storage. Locks in
the principle that no service runs at host scope, that all storage
allocation flows through Atrium-managed daemons, and that
filesystem backend is an operator deployment choice rather than an
Atrium-imposed dependency.

Companion specs:
- `docs/spec/portcullis.md` — jail launcher + capability manifest
- `docs/spec/jaild-policy.md` — privileged-broker allow-list
- `docs/spec/login-handoff.md` — boot-to-session protocol
- `docs/spec/service-management.md` — decomposition principle

## 1. Principle

> **The only legitimate operator action at host scope is the OS
> install itself; everything ongoing happens inside an
> Atrium-managed jail.**

Three immediate consequences:

1. **No host-side `pkg install <service>`.** Third-party services
   (mysqld, postgres, nginx, …) don't go into FreeBSD's `rc.d`
   alongside Atrium. They're installed via `atrium-pkg` (D2.5
   future work) into a shape Atrium can launch, and they run
   jailed.
2. **No host-side per-service `zfs create` / `mkdir`.** Operators
   can create a *pool* once at install time (just like FreeBSD's
   installer creates `zroot`), but per-jail dataset/directory
   lifecycle is `atrium-volumes`' responsibility.
3. **No host-side `service mysql-server start`.** Services are
   reached via `portcullis-cli` or via their `aqueduct` socket —
   never via FreeBSD's host `rc(8)`.

The host service set is exactly: kernel, FreeBSD essentials
(sshd if remote admin is desired; `dhclient`/`ntpd`/`syslogd`
per service-management.md §4.x), and `atrium-jaild`. Everything
else is jailed.

## 2. Architecture overview

```
┌──────────────────────────────────────────────────────────────────┐
│ portcullisd                                                      │
│   - reads service manifests at boot                              │
│   - sees [[volumes]] declarations                                │
│   - asks atrium-volumes to provision                             │
│   - asks jaild to launch with mounts                             │
│   - mediates runtime AttachMount/DetachMount capability gates    │
└──┬───────────────────────────────────────────────────────────┬───┘
   │ aqueduct                                                  │ jaild socket
   ▼                                                           ▼
┌────────────────────────────────────┐         ┌──────────────────────────┐
│ atrium-volumes (new)               │         │ jaild                    │
│   - owns volume allocation         │         │   - performs mount()     │
│     lifecycle                      │         │     calls (create-time + │
│   - per-backend plugins            │         │     runtime)             │
│     (tessera / zfs / plain / tmpfs)│         │   - per-jail mount       │
│   - returns host paths             │         │     inventory in state   │
│     (does NOT mount)               │         │     file                 │
└────────────────────────────────────┘         └──────────────────────────┘
         │                                              │
         │  (zfs create, tessera mkdir, etc.            │  (nmount(2), nullfs/
         │   produces directory at host path)           │   tmpfs onto jail's
         ▼                                              ▼   chroot path)
┌──────────────────────────────────────────────────────────────────┐
│ Operator's mounted host filesystem                               │
│   /var/lib/atrium/storage   (Tessera by default; ZFS or plain    │
│                              are alternatives)                   │
└──────────────────────────────────────────────────────────────────┘
```

Domain ownership is fixed:

| Concern                              | Daemon          |
|--------------------------------------|-----------------|
| Volume *allocation* (create dataset, set quota, return path) | `atrium-volumes` |
| Mount *operations* (nmount(2) at create-time + runtime)      | `jaild`          |
| Capability mediation (who can request what)                   | `portcullisd`    |
| Capability *enforcement* (in the kernel)                      | jails + jaild policy |

`atrium-volumes` and `jaild` never talk to each other directly —
they're both clients of portcullisd, which orchestrates the flow.

## 3. Volume kinds (the abstract intent)

The manifest declares volumes by *kind* — what the service needs
semantically, not what filesystem implements it. Two kinds:

### 3.1 `persistent`

Survives reboot, jail teardown, and service uninstall. Operator
explicitly destroys via `atrium-volumes-cli destroy <jail>/<vol>`
with a `--really-yes` prompt; data should never accidentally
vanish.

```toml
[[volumes]]
name      = "data"
kind      = "persistent"
mount_at  = "/var/db/mysql"
mode      = "0700"
owner_uid = 88
owner_gid = 88
size_max  = "100GiB"     # backend honours where it can
backend   = "fast-db"    # optional; references operator-named backend
privacy   = false        # optional; true = opt out of cross-domain
                         # dedup entirely (Tessera `salted` dedup
                         # domain, tessera-fs.md §20.2). Default
                         # false = `deferred` policy: dedup still
                         # converges at rest, but the jail's writes
                         # are never observably deduplicated.
```

### 3.2 `tmpfs`

Lives only as long as the jail lives. Wiped on teardown.

```toml
[[volumes]]
name     = "scratch"
kind     = "tmpfs"
size_max = "2GiB"
mount_at = "/tmp"
```

The `kind` is *what the service needs*. Backend selection
(below) is *how the operator provides it*. Service authors don't
specify ZFS-vs-Tessera-vs-anything in the kind field.

## 4. Backend kinds (the implementation tiers)

A backend is a per-kind plugin in `atrium-volumes`. Each backend
provides volume allocation on a particular underlying filesystem
or mechanism:

| Backend kind | Status | Per-volume mechanism | Dedup | Quota | Snapshot |
|--------------|--------|----------------------|-------|-------|----------|
| **`tessera`** (default) | Atrium-native, what we ship with | mkdir under Tessera mount | yes (CAS, free) | TODO (Tessera v3 work) | yes (Tessera magic dir) |
| `zfs` | Alternative for ZFS-only operators | `zfs create` per-volume dataset | block-level | yes (`quota=`) | yes (`zfs snapshot`) |
| `plain` | Last resort; no features | mkdir on whatever's mounted | no | no¹ | no |
| `tmpfs` | Always available | `mount -t tmpfs` | n/a | yes (`size=`) | n/a |

¹ A future `atrium-volumes-quota-helper` could walk `du(1)` per
volume on a schedule for soft enforcement on plain backends.
Out of scope for V0.

`tessera` is the default because:

- It's Atrium-native (kernel module ships with Atrium; no licensing/
  upstream-fork drag).
- CAS dedup at chunk level means many small jails share bytes
  freely (binaries, configs, even similar logs).
- POSIX-compliant (pjdfstest sweep passing per project memory
  `tessera_pjdfstest_sweep`); applications run unchanged.
- Snapshots work via `/.tessera/snapshots/<gen>/` magic dir.
- Performance matches or beats ZFS on multi-write fsync workloads
  per `tessera_perf_session_2026-05-03`.

`zfs` is an alternative for operators on existing ZFS
infrastructure or specifically wanting ZFS-class quota/snapshot
features for write-heavy workloads (DBs).

`plain` is the lowest-common-denominator: works on UFS, ext4 via
fuse-ext2, NFS, iSCSI LUN with whatever filesystem, etc. No
features, but universal.

The default backend choice is per-system, not per-Atrium. An
operator on a single-disk laptop can run everything on a `plain`
backend on UFS; an operator with a ZFS pool can run with `zfs`;
the canonical Atrium install has `tessera`.

### 4.1 Choosing a backend — the unit-cost rationale

The backends differ sharply in **what one volume costs**, which
should drive routing:

- A **ZFS** volume is a `zfs create` **dataset** — a real
  filesystem object (own mountpoint, property set, ARC metadata,
  boot-time mount). One pool holds many datasets, so it is *not*
  pool-per-app — but each dataset carries real per-volume overhead,
  and for a volume holding a few KiB of config that unit is
  heavyweight. Block-level dedup needs a large in-RAM DDT and is
  usually left off.
- A **Tessera** volume is a **directory** — provisioning is
  `mkdir + chown` under one shared Tessera mount (the "pool"
  equivalent is the single CAS filesystem). There is *no* per-volume
  kernel object beyond an inode; create/destroy is `mkdir`/`rm`.
  CAS dedup is free and **cross-volume**, so a hundred apps with
  similar small configs collapse to shared chunks. Quotas are a
  per-directory-tree limit (tessera-quotas.md), not a per-dataset
  property — isolation + a byte ceiling without a filesystem object.

**Guidance:**

- **Small persistent state — config, UI state, app preferences,
  session metadata/scrollback — → `tessera` (the default).** These
  are the overwhelming majority of jails. A Tessera directory per
  app is so cheap that per-app *isolation* (own dir, own `owner_uid`,
  own quota domain, own `enforce_statfs`-private mount) is the right
  call — never share a volume across apps to "save resources." This
  scales to thousands of small volumes where ZFS-dataset-per-volume
  would not.
- **Write-heavy, dedup-poor, DB-class workloads — mysqld, postgres —
  → a tuned `zfs` backend** (recordsize/logbias tuned, `refquota`,
  cheap `zfs snapshot`). The manifest still only says
  `kind = "persistent"`; the operator routes these few jails to ZFS
  by name. Best of both on one system.

> **Tessera capability status (2026-06-26).** Tessera is a working
> POSIX FS (read/write/dedup/rename/links/setattr/journal-replay;
> pjdfstest largely green) and the two former gates are now cleared:
> **`mmap`/exec work** (real custom `vop_getpages`/`putpages` — `cp` a
> binary to a Tessera mount and `exec` it, both verified in-VM), and
> **`size_max` is now enforced on Tessera** as a per-directory quota
> (`atrium-volumes` sets it at provision time via the kmod ioctl;
> reserve/release on write/truncate/unlink → `EDQUOT`, statfs-scoped,
> persistent, crash-consistent — tessera-quotas.md). So a Tessera
> volume can hold exec'd binaries and mmap'd files, and `size_max` is
> a real limit. (Remaining minor: setting a quota on an *already-
> populated* dir starts `used` at 0 — provision quotas on empty dirs.)

## 5. Named backend instances

Operators don't just pick *a kind*; they pick named instances
configured for their hardware. Multiple instances of the same
kind are common — different ZFS pools tuned differently, or a
Tessera pool for general use plus another for archives.

```toml
# /etc/atrium/volumes.policy.toml

# Default backend used when a manifest doesn't ask for anything
# specific. Tessera is the canonical Atrium choice.
[[backend]]
name    = "default"
kind    = "tessera"
root    = "/var/lib/atrium/storage"
default = true

# Operator's fast DB tier. Separate ZFS pool, tuned for small
# random writes (recordsize=16k, logbias=throughput).
[[backend]]
name        = "fast-db"
kind        = "zfs"
pool        = "atrium-fast-pool"
mount_root  = "/atrium/fastdb"

# Bulk archival storage. Different physical disks, different
# tuning.
[[backend]]
name        = "bulk"
kind        = "zfs"
pool        = "atrium-bulk-pool"
mount_root  = "/atrium/bulk"

# Plain backend over an existing UFS partition for use cases
# where neither dedup nor advanced features matter.
[[backend]]
name = "plain-on-ufs"
kind = "plain"
root = "/var/lib/atrium-plain"
```

Service manifests reference backends by **name**, not by kind:

```toml
[[volumes]]
name     = "data"
kind     = "persistent"
backend  = "fast-db"      # references operator-configured name
mount_at = "/var/db/mysql"
```

`atrium-volumes` validates the reference at manifest-validate
time (i.e., at install time, not at boot or at first launch). If
the manifest references a backend the operator hasn't
configured, the install fails with a clear message naming the
available backends.

If a manifest omits `backend`, the default-marked backend is used
(typically `default` → tessera).

## 6. Mount lifetime

### 6.1 Static mounts (the common case)

Declared in the manifest. Applied by jaild's pdfork-child *before*
`jail_attach`. Inside the jail, processes can never call
`mount(2)` — disabled by default jail config and not opted into.

This is what every system service uses. The operator/author knows
ahead of time what filesystems the service needs; they go in the
manifest; jaild applies them at jail creation.

### 6.2 Dynamic mounts (the on-demand case)

For workflows that genuinely need to attach/detach volumes on a
*running* jail — removable media, on-demand snapshot mounts,
user-attached project directories, on-demand network filesystem
mounts — jaild grows two new protocol verbs:

```rust
pub enum Request {
    // ... existing ...
    AttachMount(AttachMountRequest),
    DetachMount(DetachMountRequest),
}

pub struct AttachMountRequest {
    pub jail_name:  String,
    pub source:     String,
    pub dest:       String,    // path inside the jail's chroot
    pub kind:       MountKind, // RoNullfs / RwNullfs / Tmpfs
}

pub struct DetachMountRequest {
    pub jail_name:  String,
    pub dest:       String,
    pub force:      bool,
}
```

#### How it works on FreeBSD

A jail's chroot path on the host is just a path. If a process
*outside* the jail (with mount privileges) calls `nmount()`
targeting `<jail_root>/some/path`, the mount lands in the host's
mount table — and **the running jail sees it inside as
`/some/path`** because the lookup goes through the same vnode
tree. `enforce_statfs=2` keeps it private to that jail (other
jails and host views don't see it in `df`).

The jail itself never calls `mount(2)`; `allow.mount.*` stays
disabled. jaild has the privilege.

#### Capability mediation

```
service running in jail "atrium-photoeditor-1"
  │   atrium.toml grants:  removable_media = true
  │ aqueduct → portcullisd admin socket
  ▼
portcullisd: validates capability + the request shape
  │ jaild socket
  ▼
jaild: validates source/dest against jaild policy file (same
  │   allow-list as create-time mounts), calls nmount(2),
  │   records in per-jail runtime-mount table
  ▼
jail's filesystem view now contains the new mount
```

In-jail services never talk to jaild directly. portcullisd
remains the only jaild client per `INSTALL.md` convention.

#### Cleanup

Per-jail runtime-mount inventory in jaild's state file:

```toml
[[runtime_mounts]]
jail_name = "atrium-photoeditor-1"
source    = "/var/lib/atrium/storage/volumes/photoeditor-1/projects"
dest      = "/var/projects"
kind      = "rw_nullfs"
attached_at_unix = 1715200000
```

Atomic-replace on every attach/detach (same pattern as the
existing persistent-jails state).

Two failure modes:

1. **Service exits while mount is held**: procdesc EOF →
   portcullisd → asks jaild to detach all runtime mounts on that
   jail → jaild does `nmount(MNT_FORCE)` if needed → jail
   teardown is clean.
2. **jaild crashes**: state file lists every active mount; on
   restart, jaild reconciles with kernel mount table
   (`getfsstat(2)`), unmounts orphans whose owning jail is gone.

## 7. Manifest schema (consolidated)

Putting the four axes (kind, backend, lifetime, init) together:

```toml
# /etc/atrium/services.d/50-mysqld.toml

enabled       = true
name          = "mysqld"
path          = "/var/lib/atrium/jails/mysqld-rootfs"
children_max  = 0
devfs_ruleset = 5

# === volumes ===

# Database tables: write-heavy, dedup-poor. Operator routes to
# their fast-db backend (ZFS).
[[volumes]]
name      = "data"
kind      = "persistent"
backend   = "fast-db"
mount_at  = "/var/db/mysql"
size_max  = "100GiB"
mode      = "0700"
owner_uid = 88
owner_gid = 88

# First-run setup; runs inside the jail with the volume mounted,
# before the service starts. Sentinel file dropped on success;
# subsequent boots skip. Per Portcullis spec §3.4.
[volumes.data.init]
command = "/atrium/app/libexec/mysql/mysql_install_db --datadir=/var/db/mysql"

# Logs: append-mostly, fsync-light. No backend specified →
# operator default (tessera).
[[volumes]]
name      = "logs"
kind      = "persistent"
mount_at  = "/var/log/mysql"
mode      = "0750"
owner_uid = 88
owner_gid = 88

# Scratch: ephemeral.
[[volumes]]
name     = "scratch"
kind     = "tmpfs"
size_max = "2GiB"
mount_at = "/tmp"

# === exec ===
[exec]
path = "/atrium/app/bin/mysqld_safe"
argv = ["mysqld_safe",
        "--datadir=/var/db/mysql",
        "--socket=/var/run/aqueduct/mysqld/mysqld.sock"]
uid  = 88
gid  = 88

# === network (separate spec; documented here for completeness) ===
[network]
mode = "lo0_alias"
addr = "127.10.0.5/32"

# === supervision (per service-management.md) ===
[supervision]
restart                       = "always"
restart_after_secs            = 5
failure_budget_secs           = 60
min_lifetime_for_success_secs = 30
```

One mysqld jail ends up with four different storage tiers
appropriate to each volume — binaries on Tessera (free dedup),
data on ZFS (write-heavy tuning + quotas), logs on Tessera
(cheap append-mostly), scratch on tmpfs.

## 8. Lifecycle walkthroughs

### 8.1 Install

```sh
atrium-pkg install mysql80-server@8.0.36
```

Behind the scenes:
1. Resolves a CAS bundle (could be from a remote registry; could
   be from a one-shot jailed `pkg install` that hands its
   resulting tree to Tessera).
2. Drops the service manifest at `/etc/atrium/services.d/`.
3. **Doesn't touch storage yet** — first launch creates volumes.

### 8.2 First launch

```sh
service atrium-portcullisd start
```

1. portcullisd reads `/etc/atrium/services.d/50-mysqld.toml`.
2. For each `[[volumes]]`: portcullisd asks atrium-volumes to
   `provision` the volume. atrium-volumes:
   - Looks up the requested backend by name.
   - For `kind = "persistent"`: dispatches to backend plugin
     (`zfs`-plugin runs `zfs create`; `tessera`-plugin runs
     `mkdir`; etc.).
   - Sets ownership, mode.
   - Returns the host path.
3. portcullisd builds the `CreateJailRequest` with all volumes
   as `[[mounts]]`.
4. portcullisd asks jaild to launch.
5. jaild's pdfork-child applies the static mounts, attaches the
   jail, **but doesn't yet exec the service**.
6. **Setup phase**: portcullisd checks for the `data/.atrium-init-done`
   sentinel. Absent → runs the manifest's `init.command` inside
   the jail; sentinel dropped on success.
7. The actual service `exec` runs.
8. Supervisor watches procdesc for lifecycle.

### 8.3 Subsequent boots

Steps 1–5 repeat. `provision` is a no-op returning the existing
path. Init sentinel present → setup phase skipped. Service
starts directly.

### 8.4 Snapshot (operator action)

```sh
atrium-volumes-cli snapshot mysqld/data
```

Dispatches to the backend plugin. ZFS plugin runs `zfs snapshot`.
Tessera plugin uses the magic-dir mechanism. Plain plugin returns
"snapshots not supported on this backend." Behaviour follows the
backend's capabilities; fails closed where unsupported.

### 8.5 Dynamic attach (USB drive plugged in, hypothetical)

1. atrium-devevents notices new `/dev/da0`.
2. atrium-storage daemon (or future `atrium-removable`) decides
   ownership: "this drive is for user N's session."
3. Asks portcullisd: "attach `/dev/da0p1` (formatted UFS) into
   user N's supervisor jail at `/media/usb`."
4. portcullisd validates: user N's session has the
   `removable_media` capability granted; the request shape is
   sane.
5. portcullisd asks jaild: `AttachMount { jail = "user-N-supervisor",
   source = "/dev/da0p1", dest = "media/usb", kind = "rw_nullfs" }`.
6. jaild validates against policy, runs `nmount(2)` at the jail's
   chroot path.
7. The user's file manager, running inside the supervisor jail,
   sees the new mount.

Eject reverses it.

### 8.6 Uninstall

```sh
atrium-pkg uninstall mysql80-server
```

- Removes the manifest.
- The CAS bundle's ref-count goes down (Tessera GCs when zero
  refs).
- **Persistent volumes survive** — uninstall doesn't touch
  data. Operator removes them explicitly:
  ```sh
  atrium-volumes-cli destroy mysqld/data --really-yes
  ```

## 9. atrium-volumes daemon design

Per `service-management.md` §5 decision rule, this is a separate
daemon: storage-allocation domain, integrity-isolated from
policy.

Discipline (per `LANGUAGE-POLICY.md` smallest-TCB carve-out for
privileged daemons):
- Rust (`portcullis/atrium-volumes/`).
- `#![deny(unsafe_code)]` at root; localised `mod ffi` for the
  ZFS-shellout / `mkdir(2)` / `mount(2)` calls (latter delegated
  to jaild — see §10).
- No async runtime. Single-threaded blocking accept loop, same
  shape as jaild.
- Aqueduct service at `/var/run/aqueduct/atrium-volumes.sock`.
  Only portcullisd connects.

Wire protocol (length-prefixed JSON, same shape as jaild):

```rust
pub enum Request {
    Ping,
    Provision(ProvisionRequest),
    Destroy(DestroyRequest),
    Snapshot(SnapshotRequest),
    Status(StatusRequest),
}

pub struct ProvisionRequest {
    pub jail_name: String,
    pub volume:    VolumeSpec,
}
pub struct VolumeSpec {
    pub name:      String,
    pub kind:      VolumeKind,    // Persistent / Tmpfs
    pub backend:   Option<String>, // operator-configured name; None = default
    pub mount_at:  String,
    pub mode:      u16,
    pub owner_uid: u32,
    pub owner_gid: u32,
    pub size_max:  Option<u64>,
}

pub enum Response {
    Ok,
    Provisioned { host_path: String },
    AlreadyProvisioned { host_path: String },
    BackendUnavailable { name: String, configured: Vec<String> },
    BackendDoesNotSupport { feature: String },
    Error { detail: String },
}
```

Backend plugins are compile-time selected (Cargo features):
`tessera`, `zfs`, `plain`, `tmpfs`. The `zfs` plugin shells out
to `zfs(8)`; the others use libc directly.

Persistent state at `/var/run/atrium/atrium-volumes.state.toml`:
volume inventory (jail/name → backend, host path, allocated
bytes). Atomic-replace on every change.

## 10. jaild protocol extension

`jaild::protocol::Request` grows two variants documented above
(§6.2). Validation reuses the existing mount-source allow-list;
no new policy schema needed.

`jaild::state::PersistentState` grows a `runtime_mounts` table
alongside the existing `jails` table. Same atomic-replace
semantics.

ffi: `nmount(2)` is already wrapped (`nullfs_mount`,
`tmpfs_mount`). For `unmount(2)`, add a small `ffi::unmount(path,
flags)` helper.

Effort estimate: ½ day, well-bounded.

## 11. portcullisd capability mediation

`atrium.toml` (per-app manifest, distinct from system service
manifests at `/etc/atrium/services.d/`) gains capability fields:

```toml
[capabilities]
# Permission to ask portcullisd for runtime mounts of
# user-attached removable media. Without this, the service
# can never attach mounts dynamically.
removable_media = true

# Permission to ask portcullisd for snapshot mounts (for backup
# tools).
mount_snapshots = true
```

portcullisd's admin aqueduct socket (already exists per
service-management.md §4.3) gains corresponding RPCs that
require the matching capability. portcullisd validates, then
relays `AttachMount` / `DetachMount` to jaild.

## 12. Operator install flow

For the canonical Atrium install:

```sh
# At install time (one-time):
atrium-installer create-storage /dev/da0p4
# Behind the scenes:
#   - Creates a Tessera-formatted volume on /dev/da0p4
#   - Mounts at /var/lib/atrium/storage
#   - Adds /etc/fstab entry for boot
#   - Writes /etc/atrium/volumes.policy.toml with:
#       [[backend]] name=default kind=tessera root=/var/lib/atrium/storage default=true
```

For a ZFS-using operator (e.g., porting an existing FreeBSD
system):

```sh
# Operator does once:
zpool create atrium-pool mirror da0 da1
zfs create atrium-pool/storage
zfs set mountpoint=/var/lib/atrium/storage atrium-pool/storage

# Edit /etc/atrium/volumes.policy.toml:
[[backend]]
name    = "default"
kind    = "zfs"
pool    = "atrium-pool"
mount_root = "/var/lib/atrium/storage"
default = true

[[backend]]
name        = "fast-db"
kind        = "zfs"
pool        = "atrium-fast-pool"
mount_root  = "/atrium/fastdb"
```

For a single-disk laptop with UFS:

```sh
# Operator does once (probably via FreeBSD installer):
newfs /dev/da0p4
mount /dev/da0p4 /var/lib/atrium/storage
# /etc/fstab entry...

# /etc/atrium/volumes.policy.toml:
[[backend]]
name    = "default"
kind    = "plain"
root    = "/var/lib/atrium/storage"
default = true
```

Three deployments, three backend choices, **same service
manifests work unchanged** because manifests reference backends
by name (`default`, `fast-db`, …), not by kind.

## 13. Interaction with atrium-pkg

(Spec for atrium-pkg lives in a future
`docs/spec/atrium-pkg.md`; this section sketches the relevant
interaction points.)

`atrium-pkg install <app>` is conceptually equivalent to "drop
the manifest into `/etc/atrium/services.d/` (or
`/etc/atrium/apps/`) and make the CAS-bundle reachable." Doesn't
provision storage; that happens at first launch.

**Decoupling the manifest from operator deployment.** A package
shipped to a marketplace can declare `backend = "fast-db"` for a
high-write volume, but operators on systems without a `fast-db`
backend get a clear install-time error. To make packages truly
portable, a future *capability vocabulary* lets manifests name
properties instead of operator-named instances:

```toml
[[volumes]]
name      = "data"
kind      = "persistent"
needs     = "write-heavy-db"   # vocabulary entry; not an operator name
mount_at  = "/var/db/mysql"
```

Operators map vocabulary entries to their backends in
`volumes.policy.toml`:

```toml
[backend_match]
"write-heavy-db" = "fast-db"
"archive"        = "bulk"
```

Decoupling is layered on top of named backends; it's a
refinement, not a replacement.

## 14. Open questions / future work

1. ~~**Quota enforcement on `tessera` backend.**~~ Spec'd in
   [`tessera-quotas.md`](tessera-quotas.md): per-directory-tree
   quota domains, logical-bytes accounting, hard limit. The
   `size_max` field becomes real enforcement on Tessera with that
   work landed.
2. **`plain` backend quota helper.** Same shape: a sidecar that
   periodically `du`s and warns/refuses on overrun.
3. **Snapshot retention policies.** Per-volume snapshot
   retention (keep last 7 daily, 4 weekly, 12 monthly). Lives
   in `atrium-snapshot` daemon (not atrium-volumes) per
   decomposition principle.
4. **Cross-host volume migration.** `atrium-volumes-cli send` /
   `recv` analogous to `zfs send/recv`. Backend-specific:
   tessera + zfs natively support; plain falls back to rsync.
5. **VFS-level encryption per volume.** GELI or
   per-volume-key encryption via a future `atrium-encrypt`
   layer. Out of scope for V0.
6. **Multi-backend in one jail.** Already supported by the
   design — see the mysqld walkthrough (CAS + ZFS + Tessera +
   tmpfs in one jail). No further work needed.
7. **`network` field formal spec.** Adjacent but separate;
   docs/spec/network.md when we get to it.

## 15. Implementation order

When work resumes, in priority order:

1. Land jaild's `[network]` extension (½ day; unblocks DB
   services + GUI mediator daemons; orthogonal to this spec but
   listed for sequencing).
2. Land `[[volumes]]` schema in service manifests + portcullisd
   plumbing (1 day; deserialize + pass to atrium-volumes).
3. Land `atrium-volumes` daemon with `tessera` and `tmpfs`
   backends (2 days; the canonical default path).
4. Land first-run `init` lifecycle in portcullisd's launch path
   (½ day; reuses the Portcullis spec §3.4 setup-phase
   machinery).
5. Land `zfs` backend plugin (1 day; shells out to zfs(8)).
6. Land jaild's `AttachMount` / `DetachMount` protocol (½ day;
   small extension to the existing wire format).
7. Land `plain` backend (¼ day; trivial mkdir+chown).
8. Land `atrium-pkg` install path (separate spec + 2 days).

## References

- `docs/spec/portcullis.md` §3.4 (setup phase)
- `docs/spec/jaild-policy.md` (mount source allow-list)
- `docs/spec/login-handoff.md` (portcullisd's role in lifecycle)
- `docs/spec/service-management.md` §5 (decomposition decision rule)
- `docs/spec/network.md` (jail-side networking model)
- `docs/spec/atrium-volumes.md` (the volume-allocation daemon
  this spec proposes)
- `docs/spec/atrium-pkg.md` (how packages get installed into the
  shape this spec assumes)
- `portcullis/jaild/INSTALL.md` (jaild client convention)
- Project memory `tessera_perf_session_2026-05-03` (Tessera fsync perf)
- Project memory `tessera_pjdfstest_sweep` (POSIX correctness)
