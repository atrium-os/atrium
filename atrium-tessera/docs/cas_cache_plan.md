# CAS Cache — Design and Staged Implementation

Status: planning
Owner: tessera kmod
Related: dd3f829 (dirty-content buffer), 8485819 (BIO_FLUSH gate)

## 1. Problem

`tessera_fs_fetch_blob()` (kmod/tessera_fs.c:5415) is the single
entry point for "give me the bytes for this content hash." Today it:

1. Checks the pending-manifest cache (write-back; only catches
   manifests not yet on disk).
2. **Linearly scans `pack_registry`** — for each pack: read its
   sectors via `bread`, call `tessera_pack_open`, ask
   `tessera_pack_lookup` if our hash is in this pack. Stop on hit.
3. Returns ENOENT if no pack contains the hash.

Cost: O(N_packs) disk reads + parses per fetch_blob. For a workload
that sustains many small writes, N_packs grows with each commit, so
total work is O(N²).

### Where it bites

- **vop_read** of any file requires fetching its manifest blob, plus
  every chunk blob it references.
- **vop_getpages** (mmap/exec) — same.
- **append_chunked** fetches the old manifest and (for non-aligned
  appends) the trailing partial chunk. ONE fetch_blob per vop_write
  in the chunked-write hot path.
- **dirent walks** fetch the dir manifest. Hit on every readdir,
  lookup, create, remove.
- **gc passes** fetch every pack's manifest to walk the live set.

### Measurement

P1 case: `dd if=/dev/random of=f bs=4k count=256 conv=fsync` to a
fresh tessera mount. Tessera = 0.9 MB/s, ZFS = 78 MB/s (with
compression off). The wallclock sits in fetch_blob's pack scan;
each of the ~192 chunked writes scans ~250 packs by the end.

## 2. Goals

- Turn `fetch_blob` from O(N_packs) into O(1) average for repeat
  hashes.
- Help every read path uniformly. No path-specific special cases.
- Bounded memory (sysctl-tunable cap, default ~16 MiB).
- No correctness change — cache is a hint; misses fall through to
  the existing scan.
- No durability change.

## 3. Non-goals

- Not a write-back cache. Pending-manifest cache already covers the
  unpublished-manifest case for writers.
- Not a page cache. The buffer cache (`bread`) handles disk-block
  caching at the GEOM level. We're caching at the higher level of
  "blob hash → location & bytes", to skip the linear pack scan.
- Not cross-mount.

## 4. Architecture

### Two-tier design

**Tier A — location cache** (small, dense, always on)
  - key: `tessera_hash_t` (32 B)
  - value: `{ pack_id (8 B), pack_extents (variable, usually inline),
              offset_in_pack (4 B), length (4 B) }`
  - per-entry overhead: ~64 B
  - default cap: 1 MiB → ~16k entries → covers the working set of
    most read workloads
  - hit path: skip the linear scan; bread the one pack, parse,
    return the blob bytes
  - miss path: existing scan, on success insert location

**Tier B — bytes cache** (larger, optional, for small hot blobs)
  - key: `tessera_hash_t`
  - value: malloc'd copy of blob bytes (≤ small_blob_cap_per_entry,
    e.g., 4 KiB — covers manifests, dirents, small chunk blobs)
  - per-entry overhead: ~32 B + bytes
  - default cap: 8 MiB
  - hit path: memcpy out, return immediately — no bread, no parse
  - miss path: fetch via Tier A or scan, insert if `len <=
    small_blob_cap_per_entry`

Both tiers share the same eviction discipline (LRU) and use a
shared rwlock. Tier B is layered on Tier A — a Tier B hit is also
a Tier A hit, so Tier B implies Tier A entry exists.

### Data structures

```
struct tessera_cas_loc_entry {
    LIST_ENTRY(tessera_cas_loc_entry) hash_link;   // hash bucket
    TAILQ_ENTRY(tessera_cas_loc_entry) lru_link;   // global LRU
    tessera_hash_t   hash;
    uint64_t         pack_id;
    uint32_t         offset_in_pack;
    uint32_t         length;
    /* multi-extent packs: store inline up to 4 extents, else flag
     * to fall through to extent resolver on the slow path */
    uint8_t          n_extents;     /* 1..4, or 0xFF = use resolver */
    tessera_pack_extent_t extents[4];
};

struct tessera_cas_byte_entry {
    LIST_ENTRY(tessera_cas_byte_entry) hash_link;
    TAILQ_ENTRY(tessera_cas_byte_entry) lru_link;
    tessera_hash_t   hash;
    uint32_t         length;
    uint8_t         *bytes;   /* malloc'd, length bytes */
};

struct tessera_cas_cache {
    struct mtx      mtx;
    /* Location tier */
    LIST_HEAD(, tessera_cas_loc_entry) loc_buckets[CAS_LOC_BUCKETS];
    TAILQ_HEAD(, tessera_cas_loc_entry) loc_lru;
    size_t          loc_count;
    size_t          loc_max;          /* sysctl-tunable */
    /* Bytes tier */
    LIST_HEAD(, tessera_cas_byte_entry) byte_buckets[CAS_BYTE_BUCKETS];
    TAILQ_HEAD(, tessera_cas_byte_entry) byte_lru;
    size_t          byte_bytes;
    size_t          byte_max_bytes;   /* sysctl-tunable */
    /* Stats */
    unsigned long   loc_hits, loc_misses, loc_inserts, loc_evicts;
    unsigned long   byte_hits, byte_misses, byte_inserts, byte_evicts;
    unsigned long   invalidations;
};
```

Bucket counts: 1024 for loc, 256 for byte (small-blob count is
much lower than loc-entry count).

### Hash bucket function

`hash[0..3]` interpreted as little-endian u32, mod bucket count.
The hashes are already cryptographic so the low bits are uniform.

## 5. Lifecycle

### Insert (location)

`tessera_cas_loc_insert(cache, hash, pack_id, offset, length, extents, n_extents)`

1. Acquire mtx.
2. Look up bucket; if exists, move to LRU head, update fields, drop
   mtx, return.
3. Else allocate entry (M_NOWAIT — give up cleanly if low memory),
   fill, insert in bucket head and LRU head, increment loc_count.
4. If loc_count > loc_max: pop LRU tail, remove from bucket, free.
5. Drop mtx.

### Insert (bytes)

`tessera_cas_byte_insert(cache, hash, bytes, length)`

1. If length > small_blob_cap_per_entry: skip (don't cache).
2. Allocate copy of bytes (M_NOWAIT). On failure: skip.
3. Acquire mtx, insert under hash bucket + LRU head.
4. Evict LRU entries until byte_bytes + length <= byte_max_bytes.
5. Drop mtx.

### Lookup (combined fetch_blob fast path)

```c
int tessera_fs_fetch_blob(tmp_, hash, **out_buf, *out_len)
{
    /* Existing pending-manifest check stays — write-side. */
    if (pending_manifest_lookup(...)) return 0;

    /* Tier B: bytes cache hit — return immediately. */
    if (cas_byte_lookup(cache, hash, out_buf, out_len)) return 0;

    /* Tier A: location cache hit — skip the pack scan. */
    if (cas_loc_lookup(cache, hash, &loc)) {
        bread the cached extents, pack_open, pack_lookup,
        memcpy out, insert into byte cache if small;
        return 0;
    }

    /* Miss: existing linear pack_registry scan. */
    rc = scan(...);
    if (rc == 0) {
        cas_loc_insert(cache, hash, found_pack, ...);
        if (length <= small_blob_cap_per_entry)
            cas_byte_insert(cache, hash, *out_buf, *out_len);
    }
    return rc;
}
```

### Invalidation

A blob's location becomes invalid when its pack is deleted or
moved. Hooks needed:

1. **Pack delete (gc_data_zone)** — for each unreferenced pack
   removed: drop ALL location entries pointing at that pack_id.
   Implementation: scan loc LRU, drop matches. O(loc_count) per
   pack delete; gc is rare so this is fine. Bytes cache: walk and
   drop matching hashes too.
2. **Pack move (repack)** — old pack_id retires, new pack_id
   appears with the same blob hashes inside. Drop loc entries by
   old pack_id; new entries get inserted on next miss.
3. **mkfs / mount** — cache starts empty, no invalidation needed.
4. **unmount** — drain everything in the cache teardown.

The bytes cache survives invalidations of the location cache (the
bytes are immutable for a given hash). The bytes cache only needs
draining on unmount, OR when the GC actually deletes a pack
containing those bytes (in which case any bytes still in cache are
no longer fetchable from disk — but they're still correct as long
as someone holds a reference).

Decision: drop bytes cache entries together with location entries
on pack delete, for simplicity. They can be re-inserted if the
content gets re-published later (re-publication produces an
identical hash → same eligibility, same eviction class).

### Memory cap enforcement

- Tier A: count-based (loc_count vs loc_max). Default loc_max =
  16384 (~1 MiB).
- Tier B: byte-based (byte_bytes vs byte_max_bytes). Default
  byte_max_bytes = 8 MiB.
- Both eviction policies: strict LRU.

### Locking

Single mutex per cache, held only across the bucket walk + LRU
update. No malloc/free under the lock. Pattern matches
`pending_manifest_lookup` (already-vetted in this codebase).

`fetch_blob` itself holds no other locks during the cache check
(the existing scan path takes a btree cursor which is internally
synchronized).

## 6. Sysctls

```
kern.tessera.cas_loc_max         (RW, default 16384)
kern.tessera.cas_byte_max_bytes  (RW, default 8 MiB)
kern.tessera.cas_small_blob_cap  (RW, default 4 KiB)
kern.tessera.cas_loc_count       (RD)
kern.tessera.cas_loc_hits        (RD)
kern.tessera.cas_loc_misses      (RD)
kern.tessera.cas_loc_evicts      (RD)
kern.tessera.cas_byte_bytes      (RD)
kern.tessera.cas_byte_hits       (RD)
kern.tessera.cas_byte_misses     (RD)
kern.tessera.cas_byte_evicts     (RD)
kern.tessera.cas_invalidations   (RD)
```

## 7. Integration points

| Site | Action |
|---|---|
| `tessera_fs_fetch_blob` | New fast path before scan; insert on miss-success |
| `tessera_fs_publish_*_to_disk` | Insert location entry post-publish (we know exactly where the blob landed) |
| `tessera_fs_gc_data_zone` (pack delete) | `cas_invalidate_pack(cache, pack_id)` per deleted pack |
| `tessera_fs_repack_*` | `cas_invalidate_pack(cache, old_pack_id)` per migrated pack |
| `tessera_fs_mountfs` (cache init) | `cas_cache_init(&tmp_->cas_cache)` |
| `tessera_fs_unmount` (cache teardown) | `cas_cache_drain(&tmp_->cas_cache)` |

## 8. Staging — commit plan

Each stage is a separate commit with build + regression suite +
targeted benchmark. Each stage is a no-op or safe-rollback by
itself.

| Stage | Scope | Rough LoC | Risk | Rollback |
|---|---|---|---|---|
| 1 | Skeleton: struct + init + teardown + sysctls. Cache exists but no callers. | ~120 | very low | revert one commit |
| 2 | Tier A insert hook in `publish_*_to_disk`. Cache fills but isn't read. | ~30 | low | gate behind sysctl `cas_enable=0` |
| 3 | Tier A read hook in `fetch_blob`. The big read win. | ~60 | medium | revert |
| 4 | Invalidation in gc_data_zone + repack. | ~50 | medium (correctness if missed) | revert + accept stale-cache-fetches-fail |
| 5 | Tier B (bytes cache). | ~80 | low (additive) | sysctl `cas_byte_max_bytes=0` disables |
| 6 | Final benchmarks + memory updates. | docs only | none | n/a |

After each stage:
- `cd /mnt/host/atrium-tessera/kmod && make` (must build clean)
- 15+ in-tree regression scripts
- 3 stress scripts (fsx, concurrent, exhaustion)
- targeted P1 measurement (4KB×256 fsync vs prior baseline)
- `pjdfstest` once at the end (the heavy POSIX correctness sweep)

## 9. Test plan

**Functional** (regression suite + pjdfstest): unchanged behavior
under normal load.

**Stress** (stress_fsx, stress_concurrent, stress_exhaustion):
shake out cache races, eviction-under-load, invalidation races.

**Performance** (random data, three filesystems):
- 4KB×64 fsync (INLINE): expect ~unchanged (already covered by
  dirty_content buffer)
- 4KB×256 fsync (chunked, P1 case): **expect 30–80× speedup**
- 1M×4 fsync: expect modest speedup (10–30%)
- 256K×1 fsync: expect modest speedup
- Read-heavy: cat 100 files in a loop, expect dramatic speedup
  (each file's fetch_blob now O(1) instead of O(N_packs))

**Crash recovery** (stress_crash_torture): cache must not affect
durability. Should pass identically with and without cache.

**Memory** (rss watch under 1GB write workload): bounded by
sysctl caps; verify steady-state rss doesn't climb past
loc_max + byte_max_bytes overhead.

## 10. Risks

- **Stale entries after pack delete** — pure correctness bug.
  Mitigated by (a) explicit invalidation on every pack delete,
  (b) on a stale-entry hit, the bread will succeed but
  pack_lookup will fail or return wrong bytes; we'll detect by
  re-checking the hash of the returned bytes against the
  requested hash and falling through to scan on mismatch.
- **Lock contention** — single mutex on the cache. If hot, can
  shard buckets later.
- **Memory pressure** — strict caps + M_NOWAIT inserts so we
  never OOM under load; misses just fall through.
- **Cache poisoning via crafted packs** — not in threat model;
  packs come from in-process tessera publishing.

## 11. Definition of done

- All 5 implementation stages committed.
- Regression suite green.
- pjdfstest green.
- P1 benchmark within 50% of ZFS (stretch: match or exceed).
- Memory note added under
  `~/.claude/projects/.../memory/project_tessera_*` summarizing
  the cache and its sysctls.
