# `atrium-volumes` daemon

**Status:** spec + implementation (V0), 2026-05-08
**Owner:** D2.5 storage track

> **Implementation status (2026-05-08):** V0 of the daemon ships in
> `portcullis/atrium-volumes/`. Wire protocol (Ping, Provision,
> Destroy, Status, ListBackends) is fully implemented and VM-verified.
> `tessera`, `plain`, `tmpfs` plugins land; `zfs` is the next plugin
> in line. Operator CLI: `atrium-volumes-cli`. rc.d service:
> `portcullis/atrium-volumes/etc/atrium-volumes`. Crash-recovery
> orphan-mount sweep on jaild restart (companion piece) shipped at
> jaild-side.
>
> The `cas` volume kind from earlier drafts was removed — every
> persistent volume on Tessera gets CAS dedup automatically; raw
> CAS-API access would be a separate primitive if a real consumer
> ever shows up. See `storage.md` §3 for the trimmed kind list.

The privileged daemon that owns volume *allocation* (creating
datasets/directories, setting quotas, returning host paths). The
companion to jaild — jaild owns mount *operations*; atrium-volumes
owns what gets mounted.

Companion specs:
- `docs/spec/storage.md` — overall storage architecture this
  daemon implements
- `docs/spec/portcullis.md` — Portcullis manifest schema
- `docs/spec/jaild-policy.md` — privileged-broker model that
  atrium-volumes follows
- `docs/spec/service-management.md` §5 — decomposition rule that
  put this in its own daemon

## 1. Why a separate daemon

Per `service-management.md` §5 decision rule:

- **Q1**: Does this operate on portcullisd's existing data
  (manifests, sessions, procdesc, capabilities)? → **No**;
  storage is its own state.
- **Q2**: Different domain (logging, scheduling, hardware,
  networking, *storage*)? → **Yes** → STOP, separate daemon.

Same shape as jaild: smallest-TCB Rust, no async, single-threaded
accept loop, aqueduct service. portcullisd is the sole client.

Concrete benefits of separation:

- **Integrity isolation**: a portcullisd compromise doesn't
  give the attacker direct ZFS / Tessera dataset control. They
  can ask atrium-volumes for things, but atrium-volumes
  validates against its policy.
- **Audit boundary**: storage operations (zfs create, mkdir,
  chown, set quota) audit-log to a separate sink from policy
  decisions; tampering with one log doesn't taint the other.
- **Plugin maintenance**: per-backend code lives behind a trait;
  adding `btrfs` or `bcachefs` later doesn't touch portcullisd.

## 2. Architecture

```
portcullisd
   │ aqueduct
   ▼
atrium-volumes (jailed)
   │
   ├── policy: /etc/atrium/volumes.policy.toml  (named backends)
   ├── state:  /var/run/atrium/volumes.state.toml (allocation registry)
   │
   └── per-backend plugin trait
        ├── tessera plugin (default)
        ├── zfs     plugin (alternative)
        ├── plain   plugin (last resort)
        └── tmpfs   plugin (always available)
```

atrium-volumes does NOT mount. It allocates and chowns. The host
path it returns is what jaild later nullfs-mounts into the jail.

## 3. Wire protocol

Length-prefixed JSON, same shape as jaild's. 64 KiB inbound cap.

```rust
pub enum Request {
    Ping,

    /// Provision a volume. Idempotent — calling for a volume
    /// that already exists returns AlreadyProvisioned with the
    /// existing host path.
    Provision(ProvisionRequest),

    /// Destroy a volume. Operator action only (portcullisd
    /// surfaces `--really-yes` prompts).
    Destroy(DestroyRequest),

    /// Snapshot a volume. Backend-dispatched; fails on backends
    /// that don't support snapshots (e.g., plain).
    Snapshot(SnapshotRequest),

    /// Per-jail volume inventory; for portcullisd to know what
    /// to mount when launching a jail.
    Status(StatusRequest),

    /// List operator-configured backends. Used by the validator
    /// at manifest-install time to give clear errors when a
    /// manifest references an unconfigured backend.
    ListBackends,

    /// Change a previously-provisioned volume's size limit.
    /// Forwarded to the backend plugin (Tessera quota update;
    /// `zfs set refquota=`; ignored on `plain`). New limit may
    /// be smaller than current usage — future writes that exceed
    /// the new limit fail with EDQUOT; existing data is not
    /// evicted to fit. See `spec/tessera-quotas.md`.
    SetSize(SetSizeRequest),

    /// Query a volume's current usage. Returns
    /// (limit, used) on backends that enforce quotas (Tessera, ZFS),
    /// or `UsageNotEnforced` on `plain` / `tmpfs`.
    QueryUsage(QueryUsageRequest),
}

pub struct ProvisionRequest {
    pub jail_name: String,
    pub volume:    VolumeSpec,
}

pub struct VolumeSpec {
    pub name:      String,
    pub kind:      VolumeKind,        // Persistent / Tmpfs
    pub backend:   Option<String>,    // operator-configured name; None = default
    pub mount_at:  String,            // path inside the jail
    pub mode:      u32,               // octal (0700 etc.)
    pub owner_uid: u32,
    pub owner_gid: u32,
    pub size_max:  Option<u64>,       // bytes; backend honours where it can
}

pub enum VolumeKind { Persistent, Tmpfs }

pub struct DestroyRequest {
    pub jail_name: String,
    pub volume:    String,
    /// Without this set, atrium-volumes refuses (data is
    /// preserved by default; explicit confirmation required).
    pub really_yes: bool,
}

pub struct SnapshotRequest {
    pub jail_name: String,
    pub volume:    String,
    pub label:     String,             // e.g., "before-upgrade-2026-05-08"
}

pub struct StatusRequest {
    pub jail_name: Option<String>,     // None = all jails
}

pub enum Response {
    Ok,
    Provisioned        { host_path: String },
    AlreadyProvisioned { host_path: String },
    Destroyed,
    Snapshotted        { snapshot_id: String },
    Status             { volumes: Vec<VolumeRecord> },
    Backends           { backends: Vec<BackendInfo> },
    BackendUnavailable { name: String, configured: Vec<String> },
    BackendDoesNotSupport { feature: String },
    PolicyDenied       { rule: String, detail: String },
    Error              { detail: String },
}

pub struct VolumeRecord {
    pub jail_name:   String,
    pub volume_name: String,
    pub backend:     String,
    pub host_path:   String,
    pub allocated_at_unix: u64,
    pub size_used_bytes: Option<u64>,
}

pub struct BackendInfo {
    pub name:       String,
    pub kind:       String,            // "tessera", "zfs", "plain", "tmpfs"
    pub default:    bool,
    pub features:   Vec<String>,       // "quota", "snapshot", "send_recv", ...
}
```

## 4. Policy file: `/etc/atrium/volumes.policy.toml`

Operator-configured. Read once at startup; restart atrium-volumes
to apply changes (same convention as jaild's policy file).

```toml
schema_version = 1

# Default backend used when a manifest doesn't specify one.
# Most deployments make this 'tessera' for the canonical
# Atrium experience. ZFS or plain are for operators with
# specific reasons.
[[backend]]
name    = "default"
kind    = "tessera"
root    = "/var/lib/atrium/storage"
default = true

[[backend]]
name        = "fast-db"
kind        = "zfs"
pool        = "atrium-fast-pool"
mount_root  = "/atrium/fastdb"

[[backend]]
name        = "bulk"
kind        = "zfs"
pool        = "atrium-bulk-pool"
mount_root  = "/atrium/bulk"

[[backend]]
name = "plain-on-ufs"
kind = "plain"
root = "/var/lib/atrium-plain"

# Validation rules atrium-volumes enforces:
# - Exactly one backend has default = true
# - Names are unique
# - kind is one of: tessera, zfs, plain, tmpfs (tmpfs implicit)
# - root / mount_root paths exist on the host
```

Two unit-testable validations:

1. Manifest references a backend → atrium-volumes' validator
   resolves the name to a configured backend or fails.
2. Manifest requests a feature (e.g., `[volumes.X.lifecycle]
   snapshot_on_stop = true`) → atrium-volumes checks the
   resolved backend's features list; rejects at manifest-install
   time if unsupported.

## 5. State file: `/var/run/atrium/volumes.state.toml`

Authoritative inventory of allocated volumes. Atomic-replace on
every change (same `<path>.tmp` + fsync + rename pattern as
jaild's state file).

```toml
schema_version = 1
written_at_unix = 1715200000

[[volume]]
jail_name        = "mysqld"
volume_name      = "data"
backend          = "fast-db"
backend_kind     = "zfs"
host_path        = "/atrium/fastdb/atrium-pool/jails/mysqld/data"
mount_at         = "/var/db/mysql"     # informational; jaild owns the actual mount
size_max         = 107374182400        # 100 GiB
allocated_at_unix = 1715000000
owner_uid        = 88
owner_gid        = 88
mode             = "0700"

[[volume]]
jail_name        = "mysqld"
volume_name      = "logs"
backend          = "default"
backend_kind     = "tessera"
host_path        = "/var/lib/atrium/storage/jails/mysqld/logs"
mount_at         = "/var/log/mysql"
size_max         = 10737418240
allocated_at_unix = 1715000000
owner_uid        = 88
owner_gid        = 88
mode             = "0750"
```

`Tmpfs` volumes are NOT in this file: ephemeral, jaild handles
mount/unmount inline at jail-launch time, no allocation state
to track.

## 6. Plugin trait

Each backend implements:

```rust
pub trait BackendPlugin: Send + Sync {
    /// One of the kind names: "tessera", "zfs", "plain", "tmpfs".
    fn kind(&self) -> &'static str;

    /// Features this backend supports. Used by validator at
    /// manifest-install time + StatusResponse.
    fn features(&self) -> &'static [&'static str];

    /// Provision a persistent volume. Idempotent.
    fn provision(&self, ctx: &PluginCtx, spec: &VolumeSpec) -> io::Result<String>;

    /// Destroy. Refuses if the host path doesn't match an entry
    /// in atrium-volumes' state file (defence against operator
    /// mistakes that would `rm -rf` arbitrary directories).
    fn destroy(&self, ctx: &PluginCtx, host_path: &str) -> io::Result<()>;

    /// Snapshot. Returns a backend-specific snapshot identifier
    /// (e.g., a ZFS snapshot name). Plugins that don't support
    /// this return io::Error with kind = Unsupported.
    fn snapshot(&self, ctx: &PluginCtx, host_path: &str, label: &str) -> io::Result<String>;
}

pub struct PluginCtx<'a> {
    pub backend_name:   &'a str,
    pub backend_config: &'a BackendConfig,    // from policy file
    pub state_dir:      &'a Path,
}
```

### 6.1 `tessera` plugin (default)

- `provision`: `mkdir -p <root>/jails/<jail>/<volume>` +
  `chown` + `chmod`. Tessera's CAS layer dedups underneath
  transparently; no special atrium-volumes work.
- `destroy`: `rm -rf <host_path>`; Tessera's CAS GC drops
  unreferenced chunks asynchronously.
- `snapshot`: `mkdir <root>/.tessera/snapshots/<label>` (current
  Tessera magic-dir convention). Returns the snapshot name.
- `features`: `["dedup", "snapshot"]` (quota = TODO until
  Tessera v3).

### 6.2 `zfs` plugin

- `provision`: `zfs create <pool>/<jail>/<vol>` + `zfs set
  quota=…` + `chown` + `chmod`.
- `destroy`: `zfs destroy -r <pool>/<jail>/<vol>`.
- `snapshot`: `zfs snapshot <pool>/<jail>/<vol>@<label>`.
- `features`: `["quota", "snapshot", "send_recv"]`.

Shells out to `zfs(8)`; the per-jail rate is tiny so process
overhead is irrelevant.

### 6.3 `plain` plugin

- `provision`: `mkdir -p <root>/jails/<jail>/<vol>` +
  `chown` + `chmod`.
- `destroy`: `rm -rf <host_path>`.
- `snapshot`: returns `Unsupported`.
- `features`: `[]`.

The ¼-day plugin. Universal compatibility, zero features.

### 6.4 `tmpfs` plugin

Doesn't actually allocate; tmpfs volumes are mounted by jaild at
jail-create time, no atrium-volumes state. The plugin exists
only to validate `tmpfs` requests against `kind`.

- `provision`: returns a sentinel host_path ("tmpfs::<jail>/<vol>");
  jaild interprets this in its mount path.
- `destroy`: nothing.
- `snapshot`: `Unsupported`.
- `features`: `[]`.

(Open question: do we even need a tmpfs plugin? Could just
short-circuit in atrium-volumes' provision dispatcher. V0 has
the plugin for symmetry; can collapse later.)

## 7. Lifecycle

### 7.1 First boot of a fresh manifest

```
portcullisd                     atrium-volumes                jaild
─────────                       ─────────────                 ─────

reads 50-mysqld.toml
sees [[volumes]]: data (fast-db), logs (default), scratch (tmpfs)

  Provision(mysqld, data) →
                             validates against policy
                             dispatches to zfs plugin
                             zfs create atrium-fast-pool/jails/mysqld/data
                             zfs set quota=100G ...
                             chown 88:88 ...
                             writes to state file
                          ← Provisioned("/atrium/fastdb/.../data")

  Provision(mysqld, logs) →
                             dispatches to tessera plugin
                             mkdir, chown, chmod
                             writes to state file
                          ← Provisioned("/var/lib/atrium/storage/...")

  Provision(mysqld, scratch) →
                             tmpfs plugin: short-circuit
                          ← Provisioned("tmpfs::mysqld/scratch")

builds CreateJailRequest with mounts:
  source=/atrium/fastdb/.../data    dest=/var/db/mysql    rw_nullfs
  source=/var/lib/atrium/.../logs   dest=/var/log/mysql   rw_nullfs
  source=ignored                    dest=/tmp             tmpfs

                                                          → CreateJail(...)
                                                          ← JailCreated{pid, fd}
```

### 7.2 Subsequent boots

```
  Provision(mysqld, data) →
                             policy resolved; lookup state file:
                             entry exists matching name + spec
                          ← AlreadyProvisioned("/atrium/fastdb/.../data")

  ... same for logs ...

continues to jaild as before
```

`Provision` is idempotent. Calling it for an existing volume is
a fast no-op returning the cached path.

If the volume's `spec` (e.g., `size_max`, `mode`, `owner_uid`)
has changed since allocation, atrium-volumes diffs and applies
where possible:
- `size_max` change → `zfs set quota=...` (zfs); ignored on
  plain.
- `mode`, `owner_uid`/`gid` change → `chown` / `chmod` on the
  existing path.
- `kind` or `backend` change → atrium-volumes refuses
  (manifest-install time error: "this would require migrating
  data from <old> to <new>; do it manually via destroy +
  re-provision"). Defensive default; no auto-migration.

### 7.3 Snapshot (operator action)

```
operator: atrium-volumes-cli snapshot mysqld/data
                                                         portcullisd
                                                         ───────────
                                                         (forwards request)

                              atrium-volumes
                              ──────────────
                              dispatches to backend plugin (zfs)
                              zfs snapshot atrium-fast-pool/jails/mysqld/data@<label>
                           ← Snapshotted("atrium-fast-pool/...@<label>")
```

Snapshot scheduling (e.g., daily snapshots with retention) is
NOT atrium-volumes' job. A future `atrium-snapshot` daemon owns
that, calling atrium-volumes' Snapshot RPC on a schedule.

### 7.4 Destroy (operator action, with confirmation)

```
operator: atrium-volumes-cli destroy mysqld/data --really-yes

                              atrium-volumes
                              ──────────────
                              looks up state entry
                              dispatches to backend plugin
                              zfs destroy -r atrium-fast-pool/jails/mysqld/data
                              removes state-file entry
                           ← Destroyed
```

If `--really-yes` is missing, atrium-volumes refuses with a
clear "rerun with --really-yes" message. Idempotent: destroying
an already-gone volume returns `Ok` (with a warning log).

### 7.5 Crash recovery

atrium-volumes' state file is the authority. On restart:

1. Read policy file (validate, list backends).
2. Read state file (load volume inventory).
3. For each volume in state, optionally verify the host_path
   still exists; log warnings for missing paths but don't
   auto-create or auto-destroy. Operator decides what to do
   with stale entries.
4. Begin accept loop.

If state file is missing (first boot, or operator wiped it):
start with empty inventory. Every Provision becomes a fresh
allocation. Pre-existing host paths from a previous run are
NOT auto-claimed; operator manually populates state file or
re-provisions everything.

## 8. CLI: `atrium-volumes-cli`

Thin client that talks to atrium-volumes' aqueduct socket.
Convenience wrapper around the protocol.

```sh
atrium-volumes-cli list                       # all volumes
atrium-volumes-cli list --jail mysqld
atrium-volumes-cli show mysqld/data           # detailed status
atrium-volumes-cli backends                   # configured backends + features
atrium-volumes-cli snapshot mysqld/data --label "before-upgrade-2026-05-08"
atrium-volumes-cli list-snapshots mysqld/data
atrium-volumes-cli destroy mysqld/data --really-yes
```

`Provision` is NOT exposed via the CLI for ad-hoc use — only
portcullisd should provision (via manifest reads). Stops
operators from creating volumes that aren't tied to any
service.

## 9. Discipline

Per `LANGUAGE-POLICY.md` smallest-TCB carve-out:

- Rust crate at `portcullis/atrium-volumes/`.
- `#![deny(unsafe_code)]` at root; localised
  `mod ffi { #![allow(unsafe_code)] }` for `mkdir`/`chown` calls
  (libc) and any future direct syscall use.
- ZFS plugin shells out to `zfs(8)` via `Command`; that's not
  unsafe Rust, just untrusted-string-formatting we have to be
  careful about (use `--` arg separators, never construct
  shell strings).
- No async runtime. Single-threaded blocking accept.
- Aqueduct service at `/var/run/aqueduct/atrium-volumes.sock`.
- Only portcullisd connects. Same `getpeereid` peer check as
  jaild.
- Run jailed (atrium-volumes itself runs in its own jail, with
  the host paths it manages mounted in via jaild's static-mount
  setup; backends needing host filesystems get them via mount,
  not by living on the host).

## 10. Security model

Volumes contain user data. atrium-volumes:

- Does NOT read volume contents — only `chown`/`chmod`/`mkdir`/
  `zfs create`/`zfs destroy`/`zfs snapshot`/`mkdir`/`rm -rf`.
- Validates all paths against the policy file's `root` /
  `mount_root` settings (volumes can only land *under* a
  configured backend's root; refuses requests with paths
  containing `..`).
- Refuses destroy on paths not in its state file (defence
  against `--really-yes` typos that would `rm -rf` random
  things).
- Logs every operation to `/var/log/atrium/atrium-volumes.log`
  with structured fields (jail_name, volume_name, op, result,
  errno).

## 11. Open questions

1. **Volume migration between backends.** Operator wants to
   move mysqld's data from `fast-db` (zfs) to `bulk` (zfs) —
   change of pools. V1 work; for V0, manual: stop service,
   destroy old, provision new with same params, copy data,
   restart.
2. **Quota soft-limit warnings.** Should atrium-volumes notice
   approaching-quota and emit a notification (via aqueduct to
   atrium-notifyd, future)? Probably yes; not in V0.
3. **Encryption at rest.** ZFS native encryption per dataset?
   Tessera-side encryption? GELI under the pool? Three layers,
   not an atrium-volumes-only decision. Open.
4. **Cross-host volume migration.** `atrium-volumes-cli send`/
   `recv` analogous to `zfs send`/`recv`. Backend-specific.
   V2+.
5. **Plugin discovery.** Compile-time selection (Cargo
   features) for V0; runtime-loadable plugins for V2 if
   third-party backends become a thing. Probably never.

## 12. Implementation order

1. Crate skeleton + `Ping` + `ListBackends` (½ day).
2. `tessera` plugin + state file + `Provision` round-trip (1 day).
3. `tmpfs` plugin + dispatch (¼ day).
4. `zfs` plugin (1 day; shellout + parsing).
5. `plain` plugin (¼ day).
6. CLI tool (½ day).
7. `Destroy` + `Snapshot` (½ day).
8. Cross-restart state-file reload + reconciliation (½ day).

Total: ~4 days for a working atrium-volumes with all four
backends. Deferring nothing material to V1 — just snapshot
scheduling, encryption, migration, and notifications.

## 13. References

- `docs/spec/storage.md` — overall storage model + decisions
- `docs/spec/portcullis.md` §0.5 — companion specs list
- `docs/spec/jaild-policy.md` — privileged-broker policy
  pattern this daemon follows
- `docs/spec/service-management.md` §5 — separation rationale
- `portcullis/jaild/INSTALL.md` — broker-discipline boilerplate
  to mirror
