# Tessera-VFS Mapping Specification

> **Status:** v1 normative draft.
> **Scope:** how POSIX file-system semantics map onto the Tessera-FS on-disk format. The on-disk format itself is in [tessera-fs.md](tessera-fs.md).

## 0. Conventions

- All references to "FreeBSD VFS" mean the in-tree `sys/sys/vnode.h` and `sys/kern/vfs_*.c` interfaces as of FreeBSD 16.0-CURRENT (matching Atrium's bring-up target).
- "Application" means any FreeBSD userspace process making POSIX system calls.
- "Spec deviations" mean places where Tessera deliberately differs from POSIX. Each is called out explicitly.

## 1. Architecture

Tessera presents a POSIX-shaped filesystem to applications, backed by the immutable CAS layer described in tessera-fs.md.

```
   ┌─ Application (open, read, write, fsync, …)         ┐
   │                                                     │
   │   ┌─ FreeBSD VFS layer (vnode ops)                  │  POSIX surface
   │   │                                                 │
   │   │   ┌─ Tessera VFS adapter (this spec)            │
   │   │   │                                             │
   ├───┴───┴───────────────────────────────────────────  ┤
   │                                                     │
   │   ┌─ Tessera core: blobs, manifests, packs, GC      │  Storage surface
   │   │   inode table, journal, free-extent map         │
   │   └─                                                │
   │                                                     │
   │   ┌─ Block device                                   │
   │   └─                                                │
   └───────────────────────────────────────────────────  ┘
```

The adapter implements the FreeBSD `vop_*` interface and translates each POSIX call into Tessera-core operations.

## 2. Mount and lifecycle

### 2.1 Mount

`mount -t tessera <device> <mountpoint>` invokes `vfs_mount`. The adapter:

1. Opens the block device read-write.
2. Reads SB_A and SB_B from blocks 0 and 1.
3. Selects the higher-generation valid superblock as the active superblock.
4. **Self-heals the dual SB pair** per tessera-fs.md §3.3 — rewrites any copy that failed to decode or is at a stale generation, so the volume leaves mount with two consistent SBs. Self-heal runs even on read-only mounts; failures are logged but do not block the mount.
5. If `last_unmount_clean = 0`, runs journal replay (tessera-fs.md §4.5). On failure to replay (corrupted journal beyond repair), mount fails with `EIO`.
6. Writes `last_unmount_clean = 0` and `last_mount_time = now` into both superblocks; bumps generation.
7. Loads the inode-table root, pack-registry root, free-extent root, and the bloom-of-blooms cache into memory.
8. Constructs a root vnode for inode 2.
9. Returns success; the mountpoint is now live.

Read-only mount (`mount -r`) skips step 6 and refuses any operation that would issue a journal record. Step 4 (self-heal) still runs — it is a maintenance action against the volume's redundancy invariant, not a user-data mutation.

### 2.2 Unmount

`umount <mountpoint>` invokes `vfs_unmount`. The adapter:

1. Returns `EBUSY` if any vnode is still in use.
2. Flushes any pending I/O.
3. Writes a final `TX_BEGIN` ; `TX_COMMIT` empty transaction so the journal head/tail advance to a known clean point.
4. Writes `last_unmount_clean = 1` to the active superblock and its backup.
5. Releases the block device.

Forced unmount (`umount -f`) may write `last_unmount_clean = 0` and skip step 3, requiring replay on next mount.

### 2.3 Sync

`sync` and `vfs_sync` flush dirty buffers and any pending transaction batches. Tessera batches multiple inode writes into one journal transaction with a soft cap (default 32 inodes or 100 ms of dirty residence, whichever first).

## 3. Directories

A directory is an inode with `mode & S_IFMT == S_IFDIR` whose `manifest_hash` points at a `DIRECTORY` manifest (tessera-fs.md §6.2).

### 3.1 Directory manifest body

The body of a `DIRECTORY` manifest is `entry_count` directory records. Each record is variable-length:

```
offset   size  field
   0       8   child_inode
   8       4   record_length         total bytes of this record (8-byte aligned)
  12       2   name_length           bytes of name (max 255)
  14       2   reserved
  16    name   name                  UTF-8, no NUL, no '/'
  ...     pad  zero-fill to 8-byte boundary
```

Records are sorted ascending by `name` (lexicographic, byte-wise on the UTF-8). `child_inode = 0` is reserved (impossible value).

Special entries `"."` and `".."` are *not* stored on disk. The VFS adapter synthesizes them at `readdir` time:
- `"."` resolves to the directory's own inode.
- `".."` resolves to the parent inode, tracked in the open-vnode state (set when the dir is looked up via `vop_lookup`; the directory's own inode does not store a parent reference).

### 3.2 lookup

`vop_lookup(dvp, vpp, cnp)` for directory `dvp` and component name `cnp`:

1. If name is `"."`, return `dvp`.
2. If name is `".."`, return the parent vnode tracked in `dvp`'s `v_data`.
3. Read `dvp`'s inode → manifest_hash.
4. Look up the directory manifest blob in the pack.
5. Binary-search the entries for `name`.
6. If found, allocate (or get cached) vnode for `child_inode`, set its parent to `dvp->v_data->inode_no`, return.
7. If not found, return `ENOENT`.

The directory manifest is cached as a parsed in-memory structure (the entries are sorted, so binary search is direct). Cache invalidates when the inode's manifest_hash changes.

### 3.3 readdir

`vop_readdir(vp, uio, cred, eofflag, ncookies, cookies)`:

1. Read directory manifest body.
2. Iterate entries. For each, write a `dirent` to `uio` with:
   - `d_fileno = child_inode`
   - `d_type` derived from the inode's mode (`DT_REG`, `DT_DIR`, `DT_LNK`, etc.) — requires reading the child inode, which is one inode-table lookup per entry. Cached at first read.
   - `d_namlen`, `d_name` from the manifest entry.
3. Synthesize `"."` and `".."` at the start.
4. Cookies are byte-offsets into the manifest body (with sentinels for `.` and `..`).

`telldir`/`seekdir` semantics rely on cookies; cookies remain valid across reads of the same directory state. If the directory is mutated (manifest_hash changes), cookies for the old hash become invalid; subsequent `readdir` with a stale cookie returns `EINVAL`.

### 3.4 mkdir

`vop_mkdir(dvp, vpp, cnp, vap)`:

1. Build an empty `DIRECTORY` manifest (entry_count = 0, body length 32). Compute its hash.
2. Allocate a new inode number from the inode table.
3. Build the inode record: `mode = vap->va_mode | S_IFDIR`, `uid/gid` per `cnp->cn_cred`, timestamps now, `size = 32`, `nlink = 1` (".." link from the parent isn't counted in v1; we deviate from BSD's `nlink = 2` for empty dirs — see §11), `manifest_hash` = empty-dir hash.
4. Build a new parent directory manifest with `dvp`'s entries plus the new entry. Compute its hash.
5. Begin transaction:
   - `PACK_PUBLISH` (or merge into an open pack) for the empty-dir manifest and the new parent-dir manifest if they're new blobs.
   - `INODE_WRITE` (new dir inode).
   - `MANIFEST_REPOINT` (parent inode → new parent dir manifest).
   - `DIR_INSERT` (parent_inode, name, child_inode) — informational; the actual change is the manifest repoint, but having the dir-insert record makes journal-replay debugging tractable.
   - `TX_COMMIT`.
6. Allocate a vnode for the new directory.

### 3.5 rmdir

`vop_rmdir(dvp, vp, cnp)`:

1. Verify `vp` is a directory and is empty (its manifest's entry_count == 0).
2. Build new parent dir manifest without the removed entry.
3. Transaction:
   - `MANIFEST_REPOINT` (parent → new manifest).
   - `INODE_FREE` (the removed dir's inode).
   - `DIR_REMOVE` (informational).
   - `TX_COMMIT`.
4. The freed inode's manifest_hash and xattr_hash blobs become unreachable; GC reclaims them later.

`rmdir` of a non-empty directory returns `ENOTEMPTY`. Directories cannot be unlinked via `unlink`; `rmdir` is the only path.

### 3.6 rename

`vop_rename(fdvp, fvp, fcnp, tdvp, tvp, tcnp)` is the most complex op. Tessera implements POSIX rename atomicity in one journal transaction:

Within the same parent directory:
1. Build a new parent-dir manifest with the entry's name swapped from `fcnp` to `tcnp`. Compute hash.
2. If `tvp` exists (target name already in use):
   - `tvp` must be deletable (regular file: ok; directory: must be empty; check matches POSIX semantics on type matching).
   - The new manifest excludes `tcnp`'s old entry; the inode for the old `tcnp` is decremented or freed.
3. Transaction with `MANIFEST_REPOINT` (parent), inode updates, `TX_COMMIT`.

Across directories:
1. Build new manifests for both `fdvp` (sans entry) and `tdvp` (with entry). Compute both hashes.
2. Handle target replacement as above.
3. Transaction with two `MANIFEST_REPOINT` records (one per directory) plus inode adjustments. Atomic at TX_COMMIT.

The atomicity contract (§9.1) is preserved: an observer either sees the rename done or not done, never partial.

Renaming a directory checks for cycles (renaming `/a/b/c` over `/a/b/c/d/e` would create a loop): walk parent chain from `tdvp` upward; if `fvp` appears, return `EINVAL`.

## 4. Regular files

A regular file is an inode with `mode & S_IFMT == S_IFREG` whose `manifest_hash` points at an `INLINE`, `CHUNK_LIST`, or `CHUNK_TREE` manifest.

### 4.1 open

`vop_open(vp, mode, cred, td, fp)`:

1. Verify access against the inode's mode bits.
2. Allocate a per-fd open-state structure:
   - `read_cache`: lazily populated as reads happen, keyed by chunk hash.
   - `write_buffer`: only allocated on first write.
   - `dirty`: bool, set when write_buffer non-empty.
3. Hold a vnode reference; vnode is freed on close when the last reference goes away.

Open is not journaled (the inode isn't modified by open).

### 4.2 read / pread

`vop_read(vp, uio, ioflag, cred)`:

1. Walk the manifest tree:
   - `INLINE`: copy from the manifest body at the requested offset.
   - `CHUNK_LIST`: binary-search entries by `logical_offset`, find chunks covering the request range, fetch chunk blobs, slice and copy.
   - `CHUNK_TREE`: descend the tree level-by-level until reaching `level=0` leaves, then proceed as `CHUNK_LIST`.
2. Read-ahead: when serving a sequential read at offset X for length L, prefetch the next chunk (or next leaf manifest) into the cache asynchronously.
3. Update `atime` if `MNT_NOATIME` is not set. Atime updates are batched; one `INODE_WRITE` record covers many atime touches within a sync window.

The read path is concurrency-safe: blob immutability means a read in flight cannot conflict with a writer; the writer publishes a *new* manifest, and an open fd retains its initial manifest snapshot until close (§4.3.4) or refresh.

### 4.3 write / pwrite

Writes are buffered in the per-fd `write_buffer` and only published as new blobs/manifest at sync points.

#### 4.3.1 Write buffer model

The buffer is a sparse, per-fd staging area. Writes to offset X length L mutate the buffer at the corresponding range. Reads via the same fd see the *combined* view: buffer where buffer covers, original manifest where it doesn't.

The buffer is sized adaptively: small files stay in heap memory; once it crosses a threshold (default 4 MiB), it's spilled to a pack-zone scratch extent (similar to anonymous swap) and tracked by extent.

#### 4.3.2 Concurrent open fds

Multiple opens of the same inode each have their own write_buffer. Each fd sees its own writes plus the underlying manifest at open time.

POSIX consistency among fds-of-same-file is per-fd: two fds writing to the same byte range produce a "last writer wins at flush time" outcome; readers from a third fd see whichever flush committed last. This matches BSD UFS behavior.

#### 4.3.3 Read-after-write consistency

Within a single fd, `write` followed by `read` returns the just-written bytes (the write_buffer is consulted first). Across fds, the new bytes become visible only after the writer's `fsync` or `close`.

#### 4.3.4 Flush at fsync / close

`fsync(fd)` or `close(fd)` triggers buffer publication:

1. Compute the file's new content: original manifest content overlaid with the write_buffer's mutations. Materialize into a stream.
2. Run CDC over the stream → list of chunks.
3. For each chunk:
   - Compute chunk hash.
   - Check pack registry / bloom-of-blooms for existing copy.
   - If absent, write the chunk blob into a current pack.
4. Build a new manifest:
   - If the new content fits in a single inline blob (≤ `min_chunk` bytes), use `INLINE`.
   - If chunks ≤ 256 (single-level cap), use `CHUNK_LIST`.
   - Else build `CHUNK_TREE` bottom-up.
5. Compute the new manifest hash.
6. Begin transaction:
   - `PACK_PUBLISH` for each new pack created (if any).
   - `INODE_WRITE` updating size, mtime, ctime, manifest_hash, gen++.
   - `MANIFEST_REPOINT` (inode → new manifest).
   - `TX_COMMIT`.
7. Free the write_buffer.

`fsync` blocks until journal blocks are durably written (the underlying block device's `BIO_FLUSH`).

`close` performs the same flush. If the fd was never written to, close is journal-free.

### 4.4 truncate / ftruncate

`vop_setattr` with a new `va_size` smaller than current:

1. Determine the new size.
2. Flush any pending writes through the offset (treat them as canonical first).
3. If the new size is mid-chunk, the affected chunk is read, sliced to the new size, written as a new blob (potentially a new chunk in a new manifest).
4. Build a new manifest covering offset 0..new_size.
5. Transaction with `INODE_WRITE` (new size, new manifest_hash) and any `PACK_PUBLISH` for the trimmed chunk.

Extending via truncate (zero-fill) creates a sparse-tail manifest. v1 represents this with a special chunk record having `flags & 0x4 = ZERO_HOLE` (no blob storage); §11 deviation.

### 4.5 mmap

`vop_getpages` for `MAP_PRIVATE`: the kernel pages-in by calling `vop_read` semantics. Modifications are private to the process and never reach disk.

`vop_getpages` / `vop_putpages` for `MAP_SHARED`:

- **Read-only** maps: mmap each chunk blob from its pack file directly (zero-copy) or copy into pagecache (if the chunk is compressed in v2). Multiple readers across processes share pages naturally via VM dedup.
- **Read-write** maps: pages are anonymous-shared (copy on first write into a per-mapping shadow). `msync(MS_SYNC)` flushes shadow pages back through the same write-buffer publish path as `write`. `munmap` after writes triggers a sync if `MAP_SHARED`.

Concurrent `MAP_SHARED` writers across processes share the shadow pages (via the VM cache), and the *last* msync wins for the published manifest. This matches BSD's existing semantics for non-Tessera filesystems.

### 4.6 unlink

`vop_remove(dvp, vp, cnp)`:

1. Read inode `vp->v_data->inode_no`. Decrement nlink.
2. Build new parent dir manifest without the entry.
3. Transaction:
   - `MANIFEST_REPOINT` (parent → new manifest).
   - `INODE_WRITE` (decremented nlink) — OR, if nlink drops to 0 and the inode has no open references, `INODE_FREE`.
4. The inode's manifest_hash and xattr_hash blobs become potentially-unreachable; GC will reclaim them when no other inode/manifest references remain.

Unlink-while-open: POSIX requires the file to remain accessible to the holder of the fd until close. Tessera implementation: the inode is marked freeable but actual `INODE_FREE` is deferred until the last vnode reference is released. The dirent is removed immediately (the file disappears from `readdir`).

### 4.7 link

`vop_link(tdvp, vp, cnp)`:

1. Read inode `vp->v_data->inode_no`. Increment nlink.
2. Build new target dir manifest with the new entry pointing at the same inode.
3. Transaction with `MANIFEST_REPOINT` (target dir) and `INODE_WRITE` (incremented nlink).

Two dirents now point at the same inode, sharing all content (including future updates). This is independent of CAS-level dedup: hard links share *the inode*, while CAS shares *blobs*. Two unrelated files with byte-identical content share blobs (one inode each); a hard link shares the inode (one set of metadata).

### 4.8 chmod, chown, chflags, utimes

`vop_setattr` with mode/uid/gid/flags/timestamp changes:

1. `INODE_WRITE` with the updated fields, `gen++`.
2. Single `TX_COMMIT`.

These do not change `manifest_hash` (file content is unchanged), so no new blob storage is consumed.

`chflags(SF_IMMUTABLE)` sets the inode's `IMMUTABLE` flag. Subsequent attempts to write/truncate/rename return `EPERM`.

### 4.9 Reflink — `vop_copy_file_range`

`copy_file_range(2)` between two Tessera vnodes is a hash-only copy. The destination inode's `manifest_hash` (or a new manifest derived from the source's, for partial ranges) is set without reading or writing any chunk bytes. Cost is O(1) for whole-file clones, O(log n) for partial ranges that touch the manifest tree.

Constraints:

- Source and destination vnodes must be on the same Tessera mount. Cross-mount `copy_file_range` falls through to the generic copy path (read-then-write).
- Source and destination must overlap in valid ranges; the destination is extended if necessary.

Three cases:

1. **Whole-file clone** (source range covers the full source file, destination is empty or is being overwritten in full):
   - Destination inode's `manifest_hash` is set to the source's `manifest_hash`.
   - Single `INODE_WRITE` transaction.
   - The two inodes now share content via CAS; modifying one (which produces a new manifest) does not affect the other.
2. **Partial-range copy at chunk boundaries**:
   - Build a new destination manifest combining source-chunks for the copied range with destination-existing-chunks for the rest.
   - Reuses chunk hashes from both sides; no new chunk blobs created.
   - Single transaction.
3. **Partial-range copy at non-chunk boundaries**:
   - The chunks at range edges may need re-chunking (the destination's edge chunks are partially overwritten by source content). The implementation reads the affected source/destination chunks, splices in memory, re-chunks via CDC, and writes only the new edge chunks. Interior chunks are reused by hash.

This makes `cp file1 file2` an O(1) operation independent of file size, similar to APFS clones, btrfs/XFS reflinks. Tools:

- `cp --reflink=always` works directly via `copy_file_range`.
- `tessera clone <src> <dst>` is a CLI shortcut for whole-file clone.
- `cp` without `--reflink` falls through to standard read/write, producing a separately-derived (but content-identical → CAS-deduped) destination. Same end state, more I/O.

## 5. Symlinks

A symlink is an inode with `mode & S_IFMT == S_IFLNK` whose `manifest_hash` points at a `SYMLINK` manifest.

### 5.1 Symlink manifest body

The body is the target-path string. Length is `manifest.logical_size`. No NUL-termination.

### 5.2 readlink

Read the manifest body into the user buffer. No syscall-level cost beyond the manifest fetch (typically already cached for any path that just resolved through the symlink).

### 5.3 symlink

`vop_symlink(dvp, vpp, cnp, vap, target)`:

1. Build a `SYMLINK` manifest with body = target. Compute hash.
2. Allocate inode. Build inode record.
3. Update parent directory.
4. Single transaction.

Symlinks are immutable in v1 (changing a symlink's target requires unlinking and recreating; chmod and timestamps still work). This matches BSD semantics: there's no `update symlink target` syscall.

## 6. Extended attributes

Tessera-FS supports POSIX extended attributes (`getxattr`, `setxattr`, `listxattr`, `removexattr`).

### 6.1 Storage

An inode's `xattr_hash` points at an `XATTR_STORE` manifest if any xattrs are set. The body is `entry_count` records:

```
offset   size  field
   0       4   record_length
   4       2   name_length          (≤255)
   6       2   value_length         (≤4096 inline; larger via blob hash)
   8     name  name                 UTF-8, namespace-prefixed (e.g. "user.foo")
   ...    pad
   ...   value or value_hash
```

If `value_length ≤ 4096`, the value is stored inline. Otherwise the entry stores a 32-byte blob hash of an external value blob; that blob is reachable as part of GC traversal.

### 6.2 setxattr

1. Read the existing xattr-store manifest (if any).
2. Build a new xattr-store manifest with the entry inserted/replaced.
3. Update inode's xattr_hash in `INODE_WRITE`.
4. Single transaction.

### 6.3 listxattr / getxattr / removexattr

Straightforward parses + journaled inode updates analogous to setxattr.

### 6.4 Atrium-specific xattrs

The Atrium platform uses `user.atrium.tessera.tag.<n>` xattrs for tagging (the KaryaFS `tags` array equivalent). VFS treats them as opaque; tooling on top treats them as an indexable tag set. Versioning may use `user.atrium.tessera.snapshot.<label>` similarly.

## 7. Permissions and ACLs

### 7.1 POSIX mode bits

Mode bits in the inode are honored on every access check. The check is performed by FreeBSD's `vaccess()` against the cred at vop entry; Tessera does no caching of access decisions.

Owner-write bypass for content: writes always go through the open buffer, which always runs as the opening process's cred. There's no setuid file content; the inode's `mode` includes `S_ISUID`/`S_ISGID` bits which the kernel honors at exec time as usual.

### 7.2 Capsicum

Tessera vnodes participate in the standard FreeBSD capsicum framework. Capability rights restrict per-fd ops as usual; the FS itself imposes no additional restrictions beyond mode-bit checks.

### 7.3 NFSv4 ACLs

Reserved for v2. v1 stores the POSIX-mode triple only; if an NFSv4 ACL is set via `acl_set`, return `EOPNOTSUPP`.

### 7.4 No POSIX.1e ACLs

POSIX.1e ACLs (`acl_set_file`) are not supported in v1; mode bits are the entire ACL surface.

## 8. Special files

v1 explicitly does not support:

- Device nodes (`S_IFCHR`, `S_IFBLK`).
- FIFOs (`S_IFIFO`).
- Sockets (`S_IFSOCK`).

`mknod`, `mkfifo`, `bind` (against a Tessera path) all return `EOPNOTSUPP`.

Rationale: a CAS-FS storing references to ephemeral kernel objects (device numbers, socket state) doesn't fit the architecture cleanly. Per-jail tmpfs / devfs handles the special-file needs that real applications have.

## 9. Atomicity and crash safety

### 9.1 Atomicity

A "single VFS operation" maps to a single Tessera transaction (one TX_BEGIN / TX_COMMIT pair). On TX_COMMIT, the operation is durable; before TX_COMMIT, none of its effects are visible to any subsequent mount.

POSIX-required atomic operations:

- `rename`: single transaction. ✓
- `unlink` of the last hard link with no open fds: single transaction. ✓
- `mkdir`/`rmdir`: single transaction. ✓
- `link`: single transaction. ✓
- `setattr` (chmod, chown, chflags, utimes, truncate): single transaction. ✓

### 9.2 Crash window

Between TX_COMMIT records, transactions are not durable. A crash at this boundary loses everything since the last fsync. This matches POSIX `fsync` semantics: only synced data is guaranteed.

`O_DSYNC` is treated identically to `O_SYNC` in v1 (every write through `O_DSYNC` triggers a transaction).

### 9.3 Recovery contract

After any crash, mount with replay produces a consistent volume:
- Every transaction with a valid TX_COMMIT is fully applied.
- Every transaction without TX_COMMIT is fully discarded.
- The on-disk format invariants (tessera-fs.md §1) hold post-replay.

### 9.4 fsck.tessera

A separate tool, `fsck.tessera`, performs deeper checks than mount-time replay:

- Pack header / footer / blob CRC verification.
- Pack index sorted-and-correct check.
- Manifest reachability vs. pack-bloom presence.
- Inode-table tree balance and key invariants.
- Free-extent map covers exactly the unallocated pack-zone blocks.

`fsck.tessera --repair` rebuilds derivable structures (per-pack bloom filters, the pack-registry's reachable-blob counts, the bloom-of-blooms). It cannot recover from data loss; the immutability invariant means a corrupted pack-data block destroys the blobs it held, irrevocably.

## 10. Snapshots and versioning

### 10.1 Snapshot

A snapshot is a captured manifest hash held reachable across GC. Taking one is cheap: read the target subvolume's manifest_hash; insert a GC-root entry (tessera-fs §15) with `pinned_hash` = captured manifest_hash. The GC-root list is the canonical store of snapshots; everything else is presentation.

Naming convention for the GC-root entry's `name` field is `subvol-<inode_no>/snapshots/<user_name>`. The `inode_no` is the subvolume root's inode number — stable across mounts but not across delete-and-recreate of the same path. Tools that need a more stable identifier may set a `user.atrium.subvol.uuid` xattr on the subvolume root and use that prefix instead; the FS does not require this.

Whole-volume snapshots are snapshots of the root subvolume (inode 2; `subvol-2/snapshots/<user_name>`).

A snapshot's pinned_hash counts toward GC reachability for as long as the GC-root entry exists.

### 10.2 Restore

Restoring a snapshot is a `MANIFEST_REPOINT` transaction setting the target subvolume's manifest_hash back to the snapshot's pinned_hash. One transaction; atomic.

### 10.3 .tessera pseudo-directory

Tessera reserves a top-level directory entry name `".tessera"` per mount. Its contents are *synthesized by the VFS adapter at lookup time* from the GC-root list and live volume state — they are not actual on-disk directory entries:

- `.tessera/snapshots/` — synthesized directory; entries are GC-root pins whose name matches `subvol-<inode_no>/snapshots/<user_name>`. Each entry resolves (via `readdir`) to its `pinned_hash`'s manifest content (read-only; the snapshot is itself an immutable manifest).
- `.tessera/info` — synthesized regular file; content is a JSON document describing volume statistics, regenerated on each open from in-memory state.

Because `.tessera/` is synthesized, it has no on-disk inode and no on-disk dirent. Writes to `.tessera/...` paths return `EACCES` (the namespace is observe-only). Mutations to the underlying snapshots happen via the `tessera-pin` and `tessera subvol` tools (§13 of this document).

### 10.4 Subvolumes

A subvolume is a directory inode marked with the `SUBVOL_ROOT` flag (tessera-fs §14). Snapshot, diff, and send/receive operations may target a subvolume root rather than the whole volume. Subvolumes do **not** enforce access boundaries — that's Portcullis's job.

VFS-level operations (exposed as ioctls on the directory's vnode):

- `TESSERA_IOC_SUBVOL_CREATE(path)`: allocates a new directory inode at `path`, marks `SUBVOL_ROOT`. Wraps `mkdir(2)` semantics with the additional flag set.
- `TESSERA_IOC_SUBVOL_PROMOTE`: sets `SUBVOL_ROOT` on an existing empty directory. Returns `EBUSY` if the directory is non-empty.
- `TESSERA_IOC_SUBVOL_SNAPSHOT(name)`: records the subvolume's current `manifest_hash` into the GC-root list (§10.5) under `subvol-<inode_no>/snapshots/<name>`. Atomic.
- `TESSERA_IOC_SUBVOL_ROLLBACK(target_hash)`: atomically swaps this subvolume's `manifest_hash` to `target_hash`. One `MANIFEST_REPOINT` transaction. CLI tools resolve symbolic names (snapshot names) to hashes before issuing the ioctl.
- `TESSERA_IOC_SUBVOL_DIFF(other_subvol_hash) → fd`: opens a streaming fd over which a diff stream (tessera-fs §16) is produced.
- `TESSERA_IOC_SUBVOL_SEND(target_hash, [source_hash]) → fd`: same as DIFF for explicit hash arguments.
- `TESSERA_IOC_SUBVOL_RECEIVE(fd)`: issued on the target subvolume's directory vnode. Reads a diff stream from `fd`, writes all new blobs into the local CAS, verifies all hashes, and on successful stream completion atomically repoints this subvolume's `manifest_hash` to the stream's `target_hash`. Rejected if the target vnode is not a `SUBVOL_ROOT` directory. The previous manifest hash remains pinned by the system in `__last_n_commits` for trivial rollback.

Userspace CLI mirrors these as `tessera subvol create | promote | snapshot | rollback | diff | send | receive`.

Per-jail subvolume usage (Atrium-specific): each jail's filesystem root is a subvolume. Atomic update of an installed app (Opifex) is a `subvol receive` followed by an in-place `subvol rollback` to the new manifest hash. Pre-rollback the old version remains reachable via the recent-snapshot pin (tessera-fs §15.5); rollback is `MANIFEST_REPOINT` of one inode and reverses the deploy with no application restart needed unless the binary itself changed (most updates: only data files change; processes already running stay live).

### 10.5 GC roots — pinning

Inode 1 (the GC-root anchor; tessera-fs §15) is reserved system state. **It is not reachable through the directory namespace** — no dirent in `/` or anywhere else points to it, so `find /` never visits it and `stat`/`open` against any user-supplied path cannot land on it. Access goes through the dedicated ioctls (and CLI tools that wrap them) listed below; the VFS layer rejects direct file-handle constructions that target inode 1 from userspace.

Beyond live-inode reachability, an explicit pin set retains manifest hashes from GC. Use cases:

- Holding a previous OS version reachable for rollback (§10.4).
- Pinning a snapshot for backup until externally consumed.
- Marking in-flight `tessera-receive` transfers as live before all blobs arrive.

Operations exposed via ioctls on the mount root (or via direct write to the GC-root anchor inode):

- `TESSERA_IOC_PIN_ADD(name, hash, expires_ns)`: appends a GC-root entry. `expires_ns = 0` means never expires.
- `TESSERA_IOC_PIN_REMOVE(name)`: removes the named entry.
- `TESSERA_IOC_PIN_LIST`: returns the current pin list.

CLI: `tessera-pin add | rm | list`. Each mutation goes through one inode-write transaction (the GC-root anchor is inode 1).

Implicit (system-managed) pins, also visible via `tessera-pin list` with `SYSTEM` flag set, are populated by the FS itself:

- `__current_root` — the active root manifest hash.
- `__last_n_commits` — recent root manifest hashes (default N=3, retention 7 days).
- `__open_xfers/<id>` — temporary pins for in-flight `tessera-receive`.

Users SHOULD NOT name their pins with leading `__` to avoid collision.

## 11. POSIX deviations

Places where Tessera-FS deliberately differs from BSD UFS or POSIX:

| Area | UFS / POSIX | Tessera | Rationale |
|------|-------------|---------|-----------|
| Empty directory `nlink` | 2 (self + ".." entry) | 1 | We don't store ".." on disk; nlink reflects on-disk references only. Programs that rely on `nlink > 1 ⇒ subdir count` must use `readdir` instead. |
| Sparse files | Supported via UFS hole semantics | Supported via `ZERO_HOLE` chunk flag | Same observable behavior, different on-disk encoding. |
| File content mutation in place | Yes; writes update the same blocks | No; every flush produces new manifest + COW propagation | Architectural; user-visible cost is more allocation-on-write. Reads see the new content; concurrent readers see snapshots. |
| Device nodes / FIFOs / sockets | Supported | Not supported | See §8. |
| POSIX.1e ACLs | Optional | Not supported in v1 | §7.4. |
| `chflags SF_APPEND` semantics | Append-only writes | Same; enforced at VFS layer | No deviation in observable behavior. |
| File-content `O_TRUNC` race | UFS: in-place truncate, race-prone | Tessera: atomic manifest swap | Tessera is *stricter* than POSIX (a partial truncate is impossible). |
| `flock`/`fcntl` advisory locks | Per-vnode in-memory | Per-inode in-memory, not persisted | Identical to UFS at the surface. |
| `fdatasync` | Optional optimization | Same as `fsync` | Defer optimization to v2. |
| Inline data limit | None | ≤ `min_chunk` (default 16 KiB) | Files smaller than this skip chunking entirely. Visible only via `du --apparent-size` vs. `du`. |

These deviations are listed in `tessera-vfs.md` exactly (this document) so an implementation can audit conformance.

## 12. Performance contracts

These are guidelines for the implementation, not strict invariants.

- **Cold-cache file open**: ≤ 5 ms for a 4-level directory hierarchy with the bloom-of-blooms cache in memory.
- **Hot-cache `read` of a small (<64 KiB) file**: served from page-cache; performance parity with UFS within ±10%.
- **`fsync` of a small write**: ≤ 1 journal-block flush + 1 superblock flush ≈ 2 sync writes.
- **Directory readdir**: O(entries) with a single manifest fetch; comparable to UFS direct-block dirent readdir.
- **`mmap` of a chunked file**: per-chunk page-fault cost, no per-byte overhead beyond UFS.

### 12.1 Hardware acceleration requirements

A CAS-FS is hash-bound on the write path: every blob, every chunk, every manifest passes through SHA-256. Software SHA-256 caps single-core throughput around 500–800 MB/s; this is *the* dominant cost in any benchmark of bulk write or import. Tessera's implementation is required to use hardware-accelerated cryptographic primitives whenever the host CPU exposes them.

Required acceleration:

| Primitive | aarch64 | x86_64 | Expected single-core throughput |
| --- | --- | --- | --- |
| **SHA-256** | ARMv8 SHA-2 extensions (`SHA256H`, `SHA256H2`, `SHA256SU0/SU1`) | Intel SHA-NI (`SHA256RNDS2`, `SHA256MSG1/2`) | 2–4 GB/s |
| **CRC32** (journal/pack CRCs) | ARMv8 base `CRC32X/CRC32B` | SSE 4.2 `CRC32` | ~10× faster than table-based |
| **AES-GCM** (v2 encryption) | ARMv8 AES + PMULL | AES-NI + PCLMULQDQ | 5–10 GB/s |

The Rust ecosystem libraries (`sha2`, `crc32fast`, `aes-gcm`) auto-dispatch to hardware when compiled with the appropriate `asm` / SIMD features. Implementations must enable those features unconditionally for production builds.

If the running CPU does not expose hardware SHA-2 (e.g. ARMv8.0-A without crypto, ancient x86 pre-Goldmont/Zen), Tessera continues to function correctly with software fallback but does *not* meet the throughput targets in §12 — this is a performance regression, not a correctness issue.

### 12.2 Hypervisor passthrough

For dev/test under virtualization, the guest must see hardware SHA-2 in its CPU feature registers (`ID_AA64ISAR0_EL1.SHA2` on aarch64; `CPUID 7:0 EBX bit 29` on x86_64). HVF on Apple Silicon and KVM on x86 both pass these through natively when not explicitly masked. For QEMU configurations that mask these bits (some HVF variants, some emulator paths), the QEMU CPU model must be patched to surface the host's SHA-2 capability — Tessera deployment under virtualization is *not* compliant if the guest is forced to software SHA-256.

A reference implementation that misses these by an order of magnitude is not Tessera-VFS-compliant; smaller misses are tracked as performance bugs without affecting compliance.

## 13. Operational tools

The implementation must provide tooling matching these functional contracts. Naming and option flags are normative (so cross-implementation scripts work); internal mechanisms are not.

### 13.1 mkfs.tessera

`mkfs.tessera <device> [--journal-size SZ] [--label NAME]` formats a Tessera-FS volume per tessera-fs.md §13.

### 13.2 fsck.tessera

`fsck.tessera <device>` runs the consistency checks described in §9.4 of this document. Modes:

- `--check` (default): read-only verification; non-zero exit on any inconsistency.
- `--repair`: rebuild derivable structures (per-pack bloom filters, registry reachable-blob counts, bloom-of-blooms cache). Cannot recover from data loss.
- `--scrub`: equivalent to `tessera scrub` (§13.4) with the volume offline.

Exit codes match `fsck(8)` conventions: 0 = clean, 1 = errors corrected, 2 = errors not corrected, 4 = errors in unmounted FS, 8 = operational error.

### 13.3 tessera-pin

`tessera-pin add <name> <hash> [--expires <duration>]` adds a GC-root pin.
`tessera-pin rm <name>` removes a pin.
`tessera-pin list [--all]` enumerates pins (`--all` includes SYSTEM-flagged implicit pins).

### 13.4 tessera scrub

`tessera scrub <mount>` walks every blob in every pack and verifies that hash matches content. Detects bit-rot. Modes:

- Online (default): runs with the FS mounted; uses idle-priority I/O, rate-limited.
- Offline: requires the volume unmounted; runs at full I/O bandwidth.

Reports: number of blobs verified, number of mismatches, locations of any mismatches (pack id + offset). Mismatches are NOT auto-corrected; they are reported and the affected packs flagged for human attention.

A daemon variant, `tessera-scrubd`, runs scheduled scrubs. Default policy: full-volume pass per week, distributed evenly across that window.

### 13.5 tessera subvol

`tessera subvol create <path>`, `tessera subvol promote <path>`, `tessera subvol snapshot <path> <name>`, `tessera subvol rollback <path> <snapshot>`, `tessera subvol diff <a> <b>`, `tessera subvol send <path> [-d <snapshot>] [-f <file>]`, `tessera subvol receive [-f <file>] <path>`. See §10.4.

### 13.6 tessera repack

`tessera repack <mount>` runs garbage collection (tessera-fs.md §11) and consolidates partially-live packs. Modes:

- `--gc`: mark-sweep only, retiring fully-dead packs; partially-live packs untouched.
- `--full`: mark-sweep + repack of any pack whose live ratio is below a threshold (default 50%). More aggressive; reclaims more space at the cost of more I/O.
- `--aggressive`: ignores the threshold; rewrites every partially-live pack. For after large deletions or before backup.

### 13.7 tessera stat

`tessera stat <mount>` emits a JSON document describing volume state: blob count, pack count, total bytes, dedup ratio, free-extent fragmentation, journal head/tail, current generation. Suitable for monitoring integrations.

### 13.8 tessera-debug

`tessera-debug <device>` is a low-level inspection tool: dumps superblocks, reads the inode table B+tree, parses pack headers, displays manifest contents. Read-only; intended for development and post-mortem analysis. Not safe to run on a mounted volume against the live device, but safe against a snapshot or a copy.

## 14. Reserved / future work

- **Online resize**: grow/shrink the volume while mounted. v1 requires offline resize via a dedicated tool.
- **Multi-device striping**: a volume spanning multiple block devices. Out of scope for v1.
- **Quotas**: per-uid disk-usage caps. v2.
- **`renameat2` flags** (`RENAME_EXCHANGE`, `RENAME_NOREPLACE`): supported by FreeBSD only via `renameat`. v2 if upstream FreeBSD adds the flag set.
- **Write-back compression**: zstd of blobs at write time. Format reserved; not implemented in v1.

## 15. Implementation guidance (non-normative)

The natural code partition:

- `tessera-core` (Rust, `no_std + alloc`): blob handling, manifest building/parsing, CDC, B+tree, journal codec, GC algorithm, pack codec. No I/O; abstract block-device trait.
- `tessera-userspace` (Rust, std): block-device implementation against a regular file or a raw partition; `mkfs.tessera`, `fsck.tessera`, `tessera-debug`, integration tests, crash-injection harness.
- `tessera-kmod` (Rust + C glue, FreeBSD module): VFS adapter (`vop_*`), mount/unmount, kernel buffer-cache integration. Imports `tessera-core` via a minimal FFI shim or an in-tree Rust→C wrapper.

This split lets ~80% of the code run as userspace tests, with the kernel module being mostly the VFS-glue layer.

---

End of v1 spec. Implementation tracks against this document; spec changes get a versioned bump (`tessera-vfs/v1.1` for backward-compatible additions, `tessera-vfs/v2` for breaking changes).
