# Tessera quotas — per-directory disk-usage limits

**Status:** design, 2026-05-08.
**Owner:** Tessera (in-kernel filesystem) + atrium-volumes plugin.

How a Tessera-mounted filesystem enforces hard disk-usage limits
on a directory tree, fulfilling the `size_max` field on
`atrium-volumes`'s `VolumeSpec` for jails backed by Tessera. Closes
[`spec/storage.md`](storage.md) §14 open question #1.

> **Scope.** This spec covers the kernel-side quota mechanism, the
> on-disk format additions, the userspace ioctl surface, and the
> atrium-volumes plugin integration. UID/GID quotas (the traditional
> `quotactl(2)` shape) are explicitly **not** in scope — jails are
> the right unit of accounting on Atrium, not the user, and a jail
> typically contains multiple uids cooperating.

## 1. Position

A jail's `[[volumes]]` block in its manifest may specify
`size_max`. atrium-volumes promises operators that the jail cannot
exceed that limit. Today, `size_max` is enforced only on the `zfs`
backend (via `zfs set refquota=`); on Tessera and `plain` it's
documentation. This spec gives Tessera real enforcement.

The mechanism is a **per-directory-tree quota domain**: a directory
can be marked as a quota root with a byte limit; all writes to
files anywhere underneath count toward that limit; exceeding it
returns `EDQUOT` synchronously.

This is the same shape ZFS provides via `refquota` and XFS provides
via project quotas. The departure is that Tessera's CAS dedup
property forces a clarifying decision (§3.2 below).

## 2. Use case

The driving case is jail volumes:

```
atrium-volumes provisions a Tessera-backed volume:
  /var/lib/atrium/storage/jails/atrium-photoeditor/data
  with size_max = 100 GiB

→ Tessera quota: this directory tree cannot exceed 100 GiB.
→ Inside the jail, writes that would exceed return EDQUOT.
→ Operator can adjust limit later without recreating the volume.
→ Operator can query current usage via atrium-volumes-cli or sysctl.
```

Secondary uses:

- **Operator-side ad-hoc quotas** — `tessera-cli quota set /var/log
  --limit 10G` for a directory tree outside the jail world.
- **Per-snapshot accounting** (V2) — separate from this spec.

## 3. Design decisions

### 3.1 Per-directory tree, not per-uid

Quota domains are **directory trees**. A directory is marked as a
quota root; all files anywhere below count to the same domain. New
subdirectories created under a quota root inherit the same domain.

Rationale: matches how atrium-volumes thinks (a volume is a
directory). Maps cleanly to `size_max`. Transparent to apps inside
the jail — they don't see quota machinery, they just see EDQUOT
when they exceed it.

UID/GID quotas (`quotactl(2)` style) are explicitly out of scope.
A jail running multiple uids should share one quota; users in the
host shouldn't have separate quotas because they don't access
Tessera as their own uid in production. If a future need emerges,
UID quotas can be layered on; the per-directory machinery is the
foundation.

### 3.2 Logical bytes, not physical (CAS-dedup-aware)

Tessera dedups content via CAS: if two files in two different
quota domains contain identical bytes, the on-disk storage is
shared. The quota counts:

| Mode | Meaning | Behavior |
|---|---|---|
| **logical** (V1 default) | Each file's logical (uncompressed, non-deduped) size charges to its domain. Two domains with the same file each pay the full size. | Predictable: operator says "100 GiB," app sees 100 GiB available. |
| physical (deferred) | Each blob's storage cost is split across domains that reference it. Adding a duplicate file in a second domain costs ~zero bytes against that domain. | Confusing: a domain's used bytes can change when *another* domain stores the same file. Hard to reason about. |

**V1 is logical.** When operator says "100 GiB," they mean "100 GiB
of file data visible to the jail." Dedup is a system-wide
efficiency gain, not a per-jail benefit. This matches ZFS
`refquota` semantics. Physical quotas would be confusing — a jail's
free space could change because of activity in an unrelated jail.

### 3.3 Hard limit, no soft / grace

A write that would exceed the limit returns `EDQUOT`
synchronously and atomically — either the entire write fits and
the quota goes up, or the write fails and the quota is unchanged.

No "soft over with grace period" (the traditional Unix
`quotactl(2)` pattern). Soft limits add operational complexity
(notifications, grace tracking, reset rules) for marginal benefit
in the jail world where the jail simply propagates EDQUOT to its
service and the service handles "out of disk."

### 3.4 Snapshots are accounted separately, not against the live quota

Tessera's snapshots (per `snapshot_test.sh`, the
`/.tessera/snapshots/<gen>/` magic dir) are point-in-time views.
The live tree's quota counts only the live data; snapshot retention
has its own accounting axis (a future `snapshot_reserve_bytes`
field on the QuotaDomain record).

This matches ZFS's `refquota` (which counts only the live filesystem)
vs. `quota` (which counts data + snapshots + clones together). For
V1 we ship the `refquota` equivalent; the snapshot-inclusive form
lands when snapshots are revisited (V2).

### 3.5 Cross-domain renames are atomic with the quota update

`rename(2)` of a file from a directory in domain A to a directory
in domain B:

1. Compute file's quota footprint
2. Refuse with `EDQUOT` if domain B's `used + footprint > limit`
3. Atomically: move the file, decrement A's used, increment B's used
4. Record the move in the journal so replay restores the same state

If the domains are equal, no quota update happens.

### 3.6 statfs is quota-scoped per MOUNT, not per path (security-relevant)

**Normative:** a mount carrying a whole-FS quota (`mount -o
tessera.quota_bytes=N`) reports **domain-logical** numbers from
`VFS_STATFS`/`statfs(2)`, not the physical pool:

- `f_blocks` = `limit_bytes / f_bsize`
- `f_bavail` = `f_bfree` = `(limit_bytes − used_bytes) / f_bsize`
  (clamped at the physical pool's free space, so a domain whose
  limit exceeds remaining pool capacity doesn't promise bytes the
  pool can't deliver)

This is the same answer ZFS gives inside a `refquota`'d dataset,
so `df` semantics are familiar. But it is **load-bearing for
security, not just predictability**: the global physical
free-space counter is a noise-free dedup existence oracle
(tessera-fs.md §20.1 channel 1 — write a candidate file, fsync,
re-read `df`; unchanged free space → those bytes already exist
somewhere on the system).

A mount *without* a whole-FS quota reports pool-physical statfs —
host-only by policy.

> **Scoping is per-mount and cannot be per-path.** `VFS_STATFS(mp,
> sbp)` receives a *mount*, not a vnode, so the filesystem has no
> way to know which directory the caller asked about. An earlier
> draft of this section said "for any path inside a quota domain",
> which promised something the interface structurally cannot
> deliver. A **per-directory** domain (§3.1, set via
> `TESSERA_IOC_QUOTA_SET`) therefore enforces its limit correctly
> but does **not** scope `statfs` — see §3.6.1.

#### 3.6.1 Per-directory quotas enforce, but do not scope statfs

`ioctl(dirfd, TESSERA_IOC_QUOTA_SET, &limit)` marks a directory as
a quota root. Verified behaviour (2026-08-04, live Tessera root):
the limit is enforced exactly at the byte, child directories
inherit the domain, space is released on unlink, and `limit = 0`
clears it. Enforcement is sound.

What it does **not** do is scope `statfs` — a path inside a
per-directory domain still sees pool-physical numbers, because of
the per-mount constraint above.

#### 3.6.2 OPEN: channel 1 inside a jail (needs a decision)

tessera-fs.md §20.1 lists free space (`statfs`) as observation
channel 1 and states it is "closed by quota-scoped statfs
(tessera-quotas.md §3.6)". As built, that closure does not reach a
Portcullis jail, and this section should not be read as claiming
it does. Two facts, both measured 2026-08-04:

1. Jails are subtrees of **one shared Tessera volume** by design
   (portcullis.md §4.1 — the choice that makes cross-jail dedup
   work, so per-app volumes are not an option). The shared mount
   therefore has no per-app whole-FS quota to scope against, and
   §3.6.1 means a per-directory domain does not scope either.
2. A jail root is `unionfs` over read-only `nullfs`, so `df`
   inside the jail is answered by **unionfs**, not Tessera.
   Measured inside such a jail, over a directory quota'd at
   10 MiB on a 25 GiB pool:

   ```
   <above>:/var/lib/atrium/…/overlay   50G   40G   9.9G   80%   /
   ```

   The `9.9G` available matches the underlying pool's `9.9G`
   exactly — unionfs **passes the underlying free space through**.
   So a jailed app can read pool free space, which is precisely
   channel 1.

Whether this is exploitable in practice depends on the
`overlays/<id>` dedup policy: with `deferred` (the default,
portcullis.md §4.1) an untrusted write always allocates, so free
space moves whether or not the content already exists, and the
oracle gets no signal. If that reasoning holds, channel 1 is
closed by the same mechanism as channel 2 and §20.1's attribution
to statfs scoping is redundant rather than load-bearing.

**This has not been verified end to end and is not settled.** It
needs either (a) a demonstration that `deferred` really does make
free-space movement content-independent for a jailed writer, or
(b) a statfs override at the jail boundary. Until one of those
lands, do not cite §3.6 as the closure for channel 1 in a jail.

### 3.7 Mount-time toggle for testing

Mount option `tessera.quota=disabled` skips all quota checks
(useful for fsx/pjdfstest perf runs and for emergency operator
recovery if a quota record gets corrupted). Default is
`tessera.quota=enabled`.

## 4. On-disk shape

### 4.1 Inode field

Each inode gains one field:

```c
struct tessera_inode {
    /* ... existing fields ... */

    /* Quota domain id this inode belongs to.
     * 0 = not in any quota domain (legacy / global).
     * Otherwise, refers to a QuotaDomain record by id. */
    uint64_t quota_domain;

    /* ... reserved[] ... */
};
```

Inheritance: when a new inode is created inside a parent that has
`quota_domain != 0`, the new inode inherits the parent's
`quota_domain`. The field is set once at create time and never
changes implicitly — moving a file across domains updates it
explicitly via the rename path.

Existing v2 inodes load with `quota_domain = 0` (no quota); inode
record evolution carved from the existing reserved field set, same
pattern as the HMAC slot reservation (`tessera_hmac_slots`).

### 4.2 QuotaDomain record

A new btree record kind, keyed by domain_id:

```c
struct tessera_quota_domain {
    uint64_t domain_id;        /* primary key, monotonic alloc */
    uint64_t root_inode_no;    /* the directory marked as quota root */
    uint64_t limit_bytes;      /* hard limit; logical bytes */
    uint64_t used_bytes;       /* current usage; logical bytes */

    /* V1: zeros. V2 will populate for inode quotas + snapshot reserve. */
    uint64_t limit_inodes;
    uint64_t used_inodes;
    uint64_t snapshot_reserve_bytes;

    /* Dedup-domain policy (tessera-fs.md §20.2). */
    uint8_t  dedup_policy;     /* 0 = global, 1 = deferred, 2 = salted */
    uint8_t  pad[7];
    uint8_t  domain_salt[32];  /* drawn at creation iff salted; else zero */

    /* Reserved for future evolution (HMAC slot; nested-domain pointer). */
    uint8_t  reserved[24];
};
```

`dedup_policy` and `domain_salt` are immutable after domain
creation — re-salting or switching policy would orphan every
already-stored chunk hash in the domain. Changing policy means
creating a new domain and copying.

Stored in the existing btree under a new key prefix (e.g.,
`'Q' + domain_id`). Domain records are small (~128 B each); even
10K active jails is ~1 MiB of metadata.

### 4.3 SB additions

The superblock gains:

```c
uint64_t next_quota_domain_id;  /* monotonic counter for new domains */
uint64_t quota_features;        /* bitmask: quota_logical_bytes (V1),
                                   quota_inodes (V2), snapshot_reserve (V2) */
```

`quota_features` lets us evolve the quota machinery without a
full format-version bump.

### 4.4 Journal records

New journal record kind for atomic quota updates:

```c
struct journal_quota_delta {
    uint8_t  kind = JOURNAL_QUOTA_DELTA;
    uint64_t domain_id;
    int64_t  delta_bytes;       /* +N for grow, -N for shrink */
    /* ... HMAC slot ... */
};
```

Quota updates always ride with the data update they correspond to.
A truncate-grow journal entry contains both the inode-extents
update *and* the quota-delta update; replay applies them together
so the on-disk state stays consistent.

## 5. Runtime mechanics

### 5.1 Write path

```
write(fd, buf, n) at offset O:
  let i = inode-of(fd)
  let new_size = max(i.size, O + n)
  let delta = new_size - i.size

  if i.quota_domain != 0 and delta > 0:
      d = lookup_domain(i.quota_domain)
      if d.used_bytes + delta > d.limit_bytes:
          return EDQUOT

      reserve(d, delta)        /* atomic check-and-add */

  perform-the-write (existing path; emits chunked-write journal
                     records, updates inode.size, etc.)
  /* Quota delta is journaled alongside the write; on commit it's
   * persisted. */
```

The reservation is atomic against concurrent writes in the same
domain via the per-domain spinlock. Multiple writers contend on
the lock, but the critical section is two memory loads + a
compare + an add — no I/O. Throughput is not meaningfully
affected.

If the write itself fails after the reservation (e.g., I/O error),
the reservation is released by the existing rollback path.

### 5.2 Truncate-up

Identical to write-extend. Reserve `(new_size - old_size)` against
the domain; refuse with EDQUOT if it would exceed.

### 5.3 Truncate-down and unlink

```
truncate(fd, new_size) when new_size < old_size:
  let i = inode-of(fd)
  let delta = old_size - new_size
  perform-the-truncate (releases extents; existing path)
  if i.quota_domain != 0:
      release(d, delta)
```

```
unlink(path):
  let i = inode-at(path)
  if i.refcount == 0 (this is the last name):
      let domain = i.quota_domain
      let size = i.size
      perform-the-unlink (existing path)
      if domain != 0:
          release(d, size)
  else:
      perform-the-unlink (just drops the name)
```

Releases never fail — they decrement the counter.

### 5.4 Cross-domain rename

```
rename(src, dst):
  let i = inode-at(src)
  let parent_dst = lookup-parent(dst)
  let old_domain = i.quota_domain
  let new_domain = parent_dst.quota_domain

  if old_domain == new_domain:
      perform-the-rename (existing path; no quota change)
      return

  /* Cross-domain. Footprint = sum of size of i and any descendants. */
  let footprint = compute_footprint(i)

  if new_domain != 0:
      d_new = lookup_domain(new_domain)
      if d_new.used_bytes + footprint > d_new.limit_bytes:
          return EDQUOT
      reserve(d_new, footprint)

  perform-the-rename (existing path; updates each affected
                      inode's quota_domain to new_domain)

  if old_domain != 0:
      release(d_old, footprint)
```

`compute_footprint` walks the subtree counting logical bytes.
Bounded by the size of the tree being moved; for files (the common
case) it's O(1). For directories with deep contents, it's O(N) but
that matches the existing `du`-like cost of touching every inode.

### 5.5 Recovery

On mount, journal replay restores quota deltas atomically with
the data updates they ride with — domain `used_bytes` stays
consistent with actual file contents.

For paranoia / first mount after upgrade, mount option
`tessera.quota=recompute` walks the live tree once at mount time
and rebuilds every domain's `used_bytes` from inode sizes. Slow
on large filesystems (~minutes per TB); operator-invoked, not
default.

### 5.6 Concurrency

Each domain has a spinlock guarding `used_bytes` and the in-flight
reservation count. Critical sections are short (no I/O); contention
exists only on the same domain.

Cross-domain operations (rename) take both domains' locks in
ascending domain_id order to avoid deadlock.

## 6. API surface

### 6.1 ioctls

A small set on a directory fd:

```c
/* Mark `dirfd`'s inode as the root of a new quota domain.
 * Errors: EEXIST if already a quota root, EBUSY if dirfd is
 * already inside another quota domain. */
struct tessera_quota_create {
    uint64_t limit_bytes;
    uint64_t domain_id_out;     /* server fills in */
    uint8_t  dedup_policy;      /* 0 global / 1 deferred / 2 salted;
                                   immutable post-create (§4.2).
                                   salt is drawn kernel-side from
                                   arc4random — callers never supply
                                   or read it. */
    uint8_t  pad[7];
};
#define TESSERA_IOC_QUOTA_CREATE _IOWR('T', 32, struct tessera_quota_create)

/* Update the limit on an existing quota domain. dirfd must be the
 * root of the domain. New limit may be smaller than current usage;
 * future writes that would exceed the smaller limit fail with
 * EDQUOT. The domain doesn't auto-evict to fit the new limit. */
struct tessera_quota_set_limit {
    uint64_t new_limit_bytes;
};
#define TESSERA_IOC_QUOTA_SET_LIMIT _IOW('T', 33, struct tessera_quota_set_limit)

/* Query current limit + usage. dirfd may be anywhere inside a
 * quota domain — the kernel walks up to the root. Returns EINVAL
 * if dirfd is not in any domain. */
struct tessera_quota_query {
    uint64_t domain_id_out;
    uint64_t limit_bytes_out;
    uint64_t used_bytes_out;
    uint64_t root_inode_no_out;
};
#define TESSERA_IOC_QUOTA_QUERY _IOR('T', 34, struct tessera_quota_query)

/* Remove the quota domain. dirfd must be the root. After this,
 * subdirectories' inodes have their quota_domain field cleared
 * lazily on next access (or eagerly via a sysctl scrubber).
 * Errors: ENOTEMPTY if used_bytes > 0 and the caller didn't pass
 * `--force` (a reserved field bit). */
struct tessera_quota_destroy {
    uint64_t flags;             /* bit 0: force (allow destroy with usage > 0) */
};
#define TESSERA_IOC_QUOTA_DESTROY _IOW('T', 35, struct tessera_quota_destroy)
```

### 6.2 sysctl observability

Read-only sysctl tree:

```
kern.tessera.quota.domains      # count of active domains
kern.tessera.quota.list         # text dump: domain_id, limit, used, root path
kern.tessera.quota.events       # ring buffer of recent EDQUOT / overrun events
```

Same shape as the existing `kern.tessera.metatrace_*` sysctls (per
the snapshot-slice-4 work).

These sysctls are **host-only**: inside a jail they return only
the jail's own domain (or `EPERM` for the list/events nodes).
Volume-wide usage data enumerates other domains' activity and
feeds the dedup existence oracle (tessera-fs.md §20.1 channel 3);
same rule as `tessera stat` (tessera-vfs.md §13.7).

### 6.3 CLI

A new `tessera-cli quota` subcommand wrapping the ioctls:

```
tessera-cli quota create <dir> --limit 100G
tessera-cli quota set <dir> --limit 200G
tessera-cli quota query <dir>
tessera-cli quota destroy <dir> [--force]
tessera-cli quota list
```

For operators outside the atrium-volumes flow (testing, ad-hoc
quotas).

## 7. atrium-volumes integration

### 7.1 tessera plugin changes

`atrium-volumes/src/plugin.rs`'s `TesseraPlugin::provision`:

```rust
fn provision(&self, jail_name: &str, vol: &VolumeSpec, root: &Path) -> Result<PathBuf> {
    let host_path = compose_host_path(root, jail_name, &vol.name)?;
    fs::create_dir_all(&host_path)?;
    chmod(&host_path, vol.mode)?;
    chown(&host_path, vol.owner_uid, vol.owner_gid)?;

    /* NEW: apply quota if size_max is set. */
    if let Some(limit) = vol.size_max {
        let dirfd = open_dir(&host_path)?;
        tessera_quota_create(dirfd, limit)
            .or_else(|e| match e.kind() {
                /* If filesystem isn't Tessera, log + continue (the
                 * volume is created without a quota; size_max
                 * downgraded to advisory). */
                ErrorKind::Unsupported => {
                    warn!("size_max ignored: {:?} not on a Tessera mount",
                          host_path);
                    Ok(())
                }
                _ => Err(e),
            })?;
    }

    Ok(host_path)
}
```

`destroy` symmetrically calls `tessera_quota_destroy(force=true)`
before removing the directory tree.

A new `set_size` method handles operator-driven limit changes:

```rust
fn set_size(&self, host_path: &Path, new_limit: u64) -> Result<()>
```

### 7.2 atrium-volumes wire protocol additions

Two new request kinds:

```rust
pub enum Request {
    // ... existing ...
    SetSize(SetSizeRequest),       // change a volume's quota limit
    QueryUsage(QueryUsageRequest), // get current used / limit
}

pub struct SetSizeRequest {
    pub jail_name:    String,
    pub volume:       String,
    pub new_size_max: u64,
}

pub struct QueryUsageRequest {
    pub jail_name: String,
    pub volume:    String,
}

// Response variants:
pub enum Response {
    // ... existing ...
    Usage { limit_bytes: u64, used_bytes: u64 },
    UsageNotEnforced,    // backend doesn't support quotas
}
```

`atrium-volumes-cli` grows `set-size` and `usage` subcommands.

### 7.3 zfs plugin (when shipped)

Symmetric semantics. `set_size` → `zfs set refquota=N`. `usage` →
`zfs get refquota,refused`. The "logical bytes, snapshots separate"
decision is automatic on ZFS via `refquota`.

### 7.4 plain plugin

`size_max` remains advisory on the plain backend (UFS has no
per-directory quota mechanism). atrium-volumes returns
`UsageNotEnforced` on `query_usage`. Documented in the volumes
spec and in operator-facing CLI output.

A future `atrium-volumes-quota-helper` could walk `du(1)`
periodically and warn (per `storage.md` §14 #2), but it's not
real enforcement.

## 8. Implementation order

| Stage | Goal | Estimate |
|---|---|---|
| 1 | Inode field + QuotaDomain record + SB feature bit; in-memory only, no enforcement. Verify v1 inodes load with `quota_domain = 0`. | ~2 days |
| 2 | Reservation + release on write/truncate/unlink; journal records. Single-domain stress test. | ~3 days |
| 3 | Quota ioctls + sysctl. `tessera-cli quota` subcommand. | ~2 days |
| 4 | Cross-domain rename. Footprint computation. Atomicity tests. | ~2 days |
| 5 | Crash-recovery test (mount option `recompute`); verify journal-replay correctness via fault injection. | ~2 days |
| 6 | atrium-volumes tessera plugin integration: provision applies quota, destroy removes it. | ~1 day |
| 7 | atrium-volumes `SetSize` + `QueryUsage` wire ops; CLI subcommands. | ~1 day |
| 8 | Stress test: 10× concurrent writers in one domain, fault injection at every reservation point, fsx with quotas active. | ~2 days |

Total: ~15 working days (3 weeks focused).

## 9. Edge cases worth calling out

### 9.1 Pre-existing files when a quota is created

`tessera_quota_create` on a directory that already has files:
walk the subtree, sum sizes, set domain's `used_bytes` to the
total. If the existing usage is already > limit, the create
succeeds anyway (operator's choice); future writes fail until
usage drops below the limit.

### 9.2 Holes / sparse files

`truncate()` to size N without writing data: we count N as the
logical size (matches `stat.st_size`). The on-disk cost is much
less due to sparseness; the operator-visible limit is the file's
nominal size. This is consistent with how ZFS `refquota` handles
sparse files.

### 9.3 Hard links across domains

Tessera supports hard links (per `tessera_pjdfstest_sweep`).
A hard link from inside a quota domain to a file outside it
counts the file's full size. A cross-domain hard link (link in A,
file in B): the file's bytes count once, in B (where the inode
lives); A pays nothing. Hard links *across* domain roots are
unusual but supported.

### 9.4 mmap writes

`MAP_SHARED` writes that extend a file via msync go through
vop_putpages → eventual chunked write. Quota check happens at
the chunk-write path; if it fails, msync returns EDQUOT and the
write doesn't land. App sees the error on msync, not on the
preceding store.

This is the same as ZFS / XFS behavior. Apps that care wrap msync
and surface EDQUOT.

### 9.5 Snapshot interaction

Snapshots reference inodes by `(inode_no, gen)`. Their data
sharing with the live tree means deleting a file in the live tree
doesn't immediately free its bytes if a snapshot retains it.

For quota purposes (V1): the live tree's bytes count toward the
domain even if a snapshot also references them. Deleting the live
copy releases the quota credit; the bytes only stay on disk
because of the snapshot. Total disk usage may exceed the domain's
sum of live `used_bytes` when snapshots are retained — that's
expected and matches `refquota` semantics on ZFS.

### 9.6 What happens if QuotaDomain record gets corrupted?

Tessera has the journal + HMAC slot for integrity. If a domain
record fails its HMAC check at load time, mount fails with
`EIO`. Operator can mount with `tessera.quota=recompute` to
rebuild it from the live tree. Rare; operator-handled.

## 10. Open questions

1. **Project IDs vs domain IDs.** XFS uses "project IDs" as the
   stable identifier for quota groups. We use `domain_id`. Should
   the field be exposed at the file level (an attribute the user
   can read) so external tools like `find -projid` work? V2; V1
   keeps it internal.

2. **Nested quota domains.** A domain inside a domain — e.g., a
   jail with size_max = 100 GiB and a sub-volume with size_max
   = 10 GiB inside it. Useful? Probably for V2; V1 disallows
   (refuses `quota_create` on a directory already inside a
   domain). Reserved space in the QuotaDomain record for a parent
   pointer.

3. **Inode quotas.** Some workloads exhaust inodes before bytes
   (mail spool with millions of tiny files). `limit_inodes` is
   reserved in the record format; V2 enables.

4. **Per-snapshot reserve.** `snapshot_reserve_bytes` field
   reserved for V2; pairs with snapshot retention work.

5. **Notification on near-quota.** Apps could subscribe to a
   "domain at 90% full" event so they can respond gracefully.
   Future: an aqueduct event channel; CLASS_ATRIUM_VOLUMES grows
   a quota-event opcode. V2.

6. **Quota cgroup-style hierarchy.** "Total quota for all
   photoeditor jails = 500 GiB; each instance can use up to 100
   GiB." Nice; not for V1; comes after nested domains.

## References

- [`spec/storage.md`](storage.md) §14 #1 — the open question this
  spec closes.
- [`spec/atrium-volumes.md`](atrium-volumes.md) — `VolumeSpec.size_max`
  field this enforces.
- [`spec/tessera-fs.md`](tessera-fs.md), [`spec/tessera-impl.md`](tessera-impl.md) —
  Tessera filesystem architecture.
- ZFS `refquota` documentation — semantic precedent for "logical
  bytes, snapshots separate."
- XFS project quotas (`xfs_quota -x`) — precedent for per-directory-
  tree quotas without UID/GID.
- POSIX `quotactl(2)` — what we are *not* implementing (UID-based
  quotas).
