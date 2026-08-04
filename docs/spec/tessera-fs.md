# Tessera-FS On-Disk Format Specification

> **Status:** v1 normative draft.
> **Scope:** byte-level on-disk format. POSIX semantics live in [tessera-vfs.md](tessera-vfs.md).

## 0. Conventions

- All multi-byte integers are little-endian.
- All hashes are SHA-256, 32 bytes.
- All structs are explicitly aligned; no implicit padding. Reserved / unused fields are zero-filled.
- "Sector" = 4096 bytes throughout. Devices reporting smaller logical-sector sizes are rejected at mount time; devices reporting 4 KiB or larger are accepted with internal alignment to multiples of 4 KiB.
- "Block" = 4096 bytes (one sector). Block addressing is in sector units throughout this spec; `block_addr = sector_no`.
- "Extent" = one or more contiguous blocks, used for variable-size allocations (pack files, large manifests).
- Magic strings are ASCII, NUL-padded. The on-disk byte order is exactly the byte order shown.

## 1. Design invariants

These are normative; an implementation that does not preserve them is not Tessera-FS.

1. **Blob immutability.** Once a blob is written and committed, its bytes never change. Modifications produce new blobs.
2. **Manifest immutability.** A manifest hash uniquely identifies a tree of bytes; the same hash always yields the same content.
3. **Single mutable layer: the inode table.** The map from inode number → current manifest hash, and the map from directory entry → inode number, are the only structures that mutate in place. Mutation goes through the journal.
4. **Pack files are sealed on commit.** A pack file's contents are append-only-then-frozen; once committed, only `gc-repack` (which writes a new pack and retires the old) modifies pack contents.
5. **All metadata is recoverable from data.** Pack indexes, bloom filters, and the pack registry can be rebuilt by scanning pack files. The journal is the only structure whose loss requires fsck-quality recovery; everything else is rebuildable.

## 2. Volume layout

A Tessera-FS volume is a sequence of zones. Each zone is a contiguous run of blocks with a fixed role.

```
   block 0      ─┐
   block 1       │  Superblock zone (dual SB, first 2 blocks)
   block 2      ─┘
   block 3 .. J ─┐  Journal zone (circular log)
   ...           │
   ...          ─┘
   block J+1 .. ─┐  Inode-table zone (B+tree, COW)
   ...           │
   ...          ─┘
   block ... .. ─┐  Pack-registry zone (B+tree of pack-file metadata)
   ...           │
   ...          ─┘
   block ... .. ─┐  Free-extent map (B+tree of free runs)
   ...          ─┘
   block ... .. ─┐  Pack-file zone (variable-length pack extents)
   ...           │
   ...          ─┘
   last 2 blocks  Superblock backup (mirrors blocks 0–1)
```

Zone sizes are recorded in the superblock. Zones never overlap. The free-extent map covers the pack-file zone only; superblock/journal/inode/registry zones are statically sized at format time.

## 3. Superblock

Two superblocks are written: at block 0 (`SB_A`) and block 1 (`SB_B`). On every commit, the superblock with the *older* generation is overwritten with new data. The superblock with the *higher* `generation` and a valid CRC is the authoritative one at mount time.

```
offset  size  field                  description
─────── ───── ──────────────────────  ────────────────────────────────────────
   0      8   magic                   "TESSERA1"
   8      4   version_major           1
  12      4   version_minor           0
  16      4   feature_flags           bit-packed; see §3.1
  20      4   incompat_flags          bit-packed; mount fails if any unknown
  24      8   generation              monotonic; higher wins
  32     16   volume_uuid             generated at format time, immutable
  48      8   total_sectors           full volume size in 4 KiB sectors
  56      4   sector_size             4096
  60      4   reserved_a              0
  64      8   journal_start           sector
  72      8   journal_length          sectors
  80      8   inode_root              sector of inode-table B+tree root
  88      8   inode_root_generation   matches the gen of the root above
  96      8   pack_registry_root      sector of pack-registry B+tree root
 104      8   pack_registry_gen       generation of pack-registry root
 112      8   free_extent_root        sector of free-extent-map B+tree root
 120      8   free_extent_gen         generation of free-extent root
 128      8   pack_zone_start         sector of first allocatable pack block
 136      8   pack_zone_length        sectors
 144      8   next_inode_no           lowest unused inode number
 152      8   total_blob_count        live blobs (advisory, repaired by GC)
 160      8   total_pack_count        live packs (advisory, repaired by GC)
 168      8   format_time             Unix nanos, set at mkfs.tessera
 176      8   last_mount_time         Unix nanos
 184      8   meta_reserve_start      sector of metadata reserve (§3.3)
 192      8   meta_reserve_length     sectors
 200      8   meta_reserve_bump       next free sector in the reserve
 208      4   last_unmount_clean      0 = clean, 1 = unclean (replay needed)
 212      4   crc32                   CRC over bytes 0..212
 216   3880   reserved                zero
4096
```

Constants:
- `version_major = 1`. Major bump = on-disk format break.
- `version_minor` increments freely for backward-compatible additions.

### 3.1 Feature flags

| bit | name              | meaning |
| --- | ----------------- | --- |
|  0  | `ENCRYPTED`       | Volume is at-rest encrypted. Requires session unlock at mount. |
|  1  | `MULTI_LEVEL_MFT` | Manifests may use the tree variant (see §6). v1 sets this always. |
|  2  | `BLOOM_V2`        | Pack bloom uses XXH3 mixing (v1 default = blake3). Backward-compat. |
| 3-31 | reserved          | must be zero |

`incompat_flags` bits use the same numbering. A mount that doesn't recognize a set incompat bit must refuse to mount (read-only is not safe; old kernels can't reason about the new structure).

### 3.2 Backup superblock

The last two blocks of the volume are SB_A_backup and SB_B_backup, written every commit alongside the primary pair. fsck consults the backup pair if both primary blocks are corrupt.

### 3.3 Metadata allocator (open issue)

The free-extent map (§9) is itself a B+tree whose nodes are allocated from the very pool it tracks. A naive commit therefore recurses: `tessera_extent_flush` walks the in-memory free-extent set and `tessera_btree_put`s each entry; each put consumes a sector via `io.alloc`, which mutates the same set we're iterating.

v1 resolves this by reserving a fixed-size **metadata reserve** at format time, immediately after `pack_zone_start`:

```
0          1         2..3        4..4+J         metadata reserve         pack zone
SB-A       SB-B      reserved    journal        (M sectors, fixed)       (rest)
```

Mutations allocate metadata-tree nodes (inode, pack-registry, and free-extent B+trees) out of the metadata reserve via a dedicated bump pointer that is *not* tracked by the free-extent allocator. The reserve is sized to ≥ ceil(W·log_F(N)) blocks where W is the maximum number of concurrent metadata writes per commit, F is the B+tree fanout, and N is the maximum live entry count — for v1 a 256-sector reserve is sufficient.

A `tessera repack` pass periodically compacts the metadata reserve and shifts unused tail back into the pack zone.

**Implementation status (round 6a):** the reserve is *not* yet carved out. Format-time metadata zone (`TESSERA_METADATA_ZONE_SECTORS = 16`) only covers the empty-tree roots; mutation paths therefore can't be implemented yet. Round 6 starts with the reserve format-time change before any vop-write code lands.

### 3.4 Self-heal at mount

The dual superblock only earns its keep if the two copies stay in sync. At mount time, after picking the authoritative SB (highest generation with valid magic + CRC), the implementation MUST:

| Observed state                                       | Action                                                |
| ---------------------------------------------------- | ----------------------------------------------------- |
| Both SBs valid, generations equal                    | nothing                                               |
| Both SBs valid, generations differ                   | rewrite the older copy from the active one            |
| One SB valid, the other fails magic / CRC            | rewrite the corrupt copy from the active one         |
| Both SBs invalid                                     | refuse to mount (consult §3.2 backup pair via fsck)   |

The rewrite is a single 4 KiB synchronous block write and runs even on read-only mounts — it's a maintenance action against the volume's redundancy invariant, not a user-data mutation. A self-heal failure (e.g. write-protected media) is logged and does not block the mount; the volume continues running off the surviving good SB.

Without self-heal, a single bit-flip on either copy silently degrades the volume to single-copy durability until the next commit cycle, at which point a power loss between the two SB writes can lose the volume entirely.

## 4. Journal

The journal is a circular log of transactions that update the mutable inode-table or pack-registry roots. Pack-file writes do *not* go through the journal (they are immutable; the journal merely records "pack `<hash>` exists"). Blob writes do *not* go through the journal (they are immutable; the pack file records them durably before the journal references them).

### 4.1 Journal layout

```
journal_start ───┐
                 │  block 0: journal header (head, tail, generation, CRC)
                 │  block 1..N-1: log records
                 │
                 │  Treated as a circular buffer; head/tail wrap.
                 ▼
journal_start + journal_length
```

### 4.2 Journal header

```
offset  size  field
   0      8   magic           "TJOURNAL"
   8      4   version         1
  12      4   reserved
  16      8   head_seq        lowest sequence number not yet retired
  24      8   tail_seq        next sequence number to assign
  32      8   head_block      block index of head record (relative to journal_start)
  40      8   tail_block      block index past the last written record
  48      4   crc32           CRC over bytes 0..48
  52   4044   reserved
```

### 4.3 Records

Every record has a 32-byte header followed by a record-type-specific body. All records are aligned to 4 KiB boundaries (one block per record); records longer than 4068 bytes occupy multiple blocks, contiguous within the journal's circular order.

Record header:

```
offset  size  field
   0      4   magic           "TXR\0"
   4      4   record_type     see §4.4
   8      8   sequence        monotonic; matches head_seq..tail_seq
  16      4   body_length     bytes after the 32-byte header
  20      4   block_count     blocks consumed by this record (>= 1)
  24      4   crc32_body      CRC32 of the body bytes
  28      4   crc32_header    CRC32 of bytes 0..28
```

A record is *durable* when its blocks have been written and fsync'd; it is *committed* when the corresponding `TX_COMMIT` record (or implicit commit) has also been durably written and the head pointer in the journal header has been updated.

### 4.4 Record types

| code | name                  | body |
| ---- | --------------------- | ---- |
|  1   | `TX_BEGIN`            | tx_id (8), parent_tx_id (8), reason_tag (16) |
|  2   | `TX_COMMIT`           | tx_id (8) |
|  3   | `TX_ABORT`            | tx_id (8), reason_code (4) |
|  4   | `INODE_WRITE`         | tx_id (8), inode_no (8), inode_record (128, see §7) |
|  5   | `INODE_FREE`          | tx_id (8), inode_no (8) |
|  6   | `MANIFEST_REPOINT`    | tx_id (8), inode_no (8), new_manifest_hash (32) |
|  7   | `DIR_INSERT`          | tx_id (8), parent_inode (8), name_len (2), name (≤255), child_inode (8) |
|  8   | `DIR_REMOVE`          | tx_id (8), parent_inode (8), name_len (2), name (≤255) |
|  9   | `PACK_PUBLISH`        | tx_id (8), pack_id (16), start_sector (8), length_sectors (8), blob_count (4), total_bytes (8) |
| 10   | `PACK_RETIRE`         | tx_id (8), pack_id (16) |
| 11   | `EXTENT_ALLOC`        | tx_id (8), start_sector (8), length_sectors (8) |
| 12   | `EXTENT_FREE`         | tx_id (8), start_sector (8), length_sectors (8) |
| 13   | `ROOT_UPDATE`         | tx_id (8), root_kind (1), new_root_sector (8), new_root_gen (8) |
| 14   | `GC_TOMBSTONE`        | tx_id (8), pack_id (16), retired_blob_count (4), retired_bytes (8) |

`root_kind` is `0 = inode_root`, `1 = pack_registry_root`, `2 = free_extent_root`.

### 4.5 Replay

On mount with `last_unmount_clean = 1`, replay is skipped.

On mount with `last_unmount_clean = 0`:
1. Read the journal header. Walk records from `head_seq` forward.
2. Group records by `tx_id`.
3. For each transaction:
   - If a `TX_COMMIT` exists with matching `tx_id` and the body CRC of every constituent record validates, the transaction is **committed**: apply each record to the in-memory state and re-write the relevant root pointers in the superblock.
   - If `TX_COMMIT` is missing, or any record's CRC fails, the transaction is **aborted**: discard.
4. Rewrite the superblock with the post-replay roots and `last_unmount_clean = 1`. The journal is then truncated by advancing `head_seq` past the replayed range.

Replay must be idempotent: multiple replays produce identical state.

## 5. Pack files

Pack files hold blob bytes. A pack is a contiguous extent of blocks within the pack-file zone; its starting sector and length-in-sectors are recorded in the pack registry (§8) and (during a publish transaction) in the journal.

### 5.1 Pack file layout

```
+-----------------------------+  pack start sector
|  Pack header (4 KiB)         |
+-----------------------------+
|  Blob index                  |  sorted by hash; (hash, offset, size)
|  (B blocks; B = ceil(N*48/4096)) |
+-----------------------------+
|  Bloom filter                |  size from header.bloom_bytes
+-----------------------------+
|  Blob data                   |  concatenated blobs, 64-byte aligned
|  ...                         |
+-----------------------------+
|  Pack footer (4 KiB)         |  CRC of header+index+bloom+data
+-----------------------------+  (next pack starts at next 4 KiB boundary)
```

### 5.2 Pack header

```
offset  size  field
   0      8   magic                "TPACK\0\0\0"
   8      4   version              1
  12      4   pack_kind            0=tiny, 1=small, 2=mixed
  16     16   pack_id              UUIDv4 generated at create
  32      8   create_time          Unix nanos
  40      8   creator_tx_id        journal tx that published this pack
  48      4   blob_count           N
  52      4   index_blocks         B
  56      4   bloom_bytes          size of bloom filter, must be page-aligned (multiple of 4096)
  60      4   bloom_hash_count     number of hash functions in bloom (typically 7)
  64      8   data_offset          byte offset from pack start to blob data
  72      8   data_length          bytes of blob data
  80      8   total_pack_bytes     bytes from header start to footer end
  88      4   crc32_header         CRC of bytes 0..88
  92   4004   reserved
4096
```

### 5.3 Blob index

Each entry is exactly 48 bytes:

```
offset  size  field
   0     32   blob_hash            SHA-256 of the blob content
  32      8   data_offset          byte offset from pack start
  40      4   data_size            bytes
  44      4   flags                bit 0 = manifest-blob, bit 1 = chunk-blob, bit 2 = inline-data
```

Entries are sorted ascending by `blob_hash`; lookup is binary search. Tail of the last index block is zero-filled.

### 5.4 Bloom filter

A standard Bloom filter sized to give ≤0.1% false-positive rate at the pack's blob count. Stored as a flat bit array, MSB-first.

The writer computes:

- `bloom_bits = max(4096 * 8, ⌈10 * blob_count⌉)` — at least one 4 KiB block, then ~10 bits per blob (gives ~0.1% FPR with optimal `k`).
- `bloom_bytes = ⌈bloom_bits / 8⌉ rounded up to 4096`. The filter occupies a whole number of blocks.
- `bloom_hash_count = max(1, ⌊(bloom_bits / blob_count) * ln(2) + 0.5⌋)` — optimal k for the chosen m/n ratio. Typically 7–10 for reasonable blob counts.

The hash function is `XXH3_128` of the 32-byte blob hash, treated as `bloom_hash_count` independent bit indexes derived from rotated subsets of the 128-bit output (Kirsch–Mitzenmacher technique).

The filter is rebuilt from the index after pack creation; corruption is recoverable. Readers MUST honor the `bloom_hash_count` field in the pack header rather than recomputing it.

### 5.5 Blob data area

Blobs are stored back-to-back. Each blob is preceded by a 16-byte blob descriptor:

```
offset  size  field
   0      4   magic                "TBLB"
   4      4   uncompressed_size    same as data_size in index unless compressed
   8      4   compressed_size      0 if not compressed
  12      4   reserved
  16    ...   blob bytes
```

Compression is reserved for v2 (`compressed_size != 0`); v1 always writes `compressed_size = 0` and stores raw bytes.

Blobs are aligned to 64-byte boundaries (cache line). Trailing padding is zero-filled and not counted in `data_size`.

### 5.6 Pack footer

```
offset  size  field
   0      8   magic                "TPACKEND"
   8      4   blob_count_check     must equal header.blob_count
  12      4   crc32_pack           CRC32 of bytes 0..(footer_offset)
  16   4080   reserved
```

A pack with a header CRC mismatch, footer mismatch, or pack CRC mismatch is treated as failed-to-publish and not entered into the registry.

### 5.7 Pack-kind distinctions

- `pack_kind = 0` (tiny): all blobs <= 4 KiB. Typical content: function blobs, small configs, manifest tree leaves.
- `pack_kind = 1` (small): all blobs <= 64 KiB. Typical content: fully-cached chunks for medium files, dirent listings.
- `pack_kind = 2` (mixed): no blob-size constraint. Used for cold-storage repacks where the access pattern doesn't warrant separation.

The kind is advisory for the writer (which packs to push a new blob into) and informative for the reader (which packs are likely-hot).

### 5.8 Pack size policy

- Default soft-cap: 64 MiB per pack.
- Hard upper bound: 1 GiB per pack (avoids 32-bit offset overflow inside pack with margin).
- Cold-tier repacks may produce larger packs up to the hard cap.
- A single pack must fit in a single contiguous extent; if free space is fragmented, the writer either chooses a smaller pack or triggers a free-extent compaction.

### 5.9 Pack lifecycle

A pack file's lifecycle is **created → published → (optionally repacked) → retired**. There is no "open / appendable" state across transactions:

1. **Created** by a writing transaction. The transaction allocates an extent, writes header + blob descriptors + blob bytes + index + bloom + footer for *exactly* the blobs being added by that transaction, and emits `PACK_PUBLISH` referencing the pack. The pack is sealed (footer-CRC'd, immutable) before the journal commit.
2. **Published**. The pack appears in the registry (§8); its blobs are reachable via the bloom-of-blooms.
3. **Retired** by `tessera repack` (§13.6 in tessera-vfs.md) when its live-blob ratio drops or when consolidation is requested. The repack process is itself a transaction: the new (consolidated) pack is created and published; the old packs are journaled `PACK_RETIRE`'d; their extents are freed in the same transaction.

This means most packs created by ordinary writes are **small** (one or a few blobs each); they accumulate until a repack consolidates them. The tiny/small/mixed `pack_kind` distinction (§5.7) is meaningful primarily for repacked output; per-transaction packs use `pack_kind = 0` (tiny) by default and are repacked into kind-1 or kind-2 packs later.

This design avoids the complexity of cross-transaction "open packs" (durability and recovery would require ordering pack-content writes against journal commits across transaction boundaries). The cost is many small packs in the steady-state until repack runs; the bloom-of-blooms keeps lookup-cost manageable until then.

## 6. Manifests

A manifest is a blob whose interpretation describes the content of one logical file (or directory, or symlink, or xattr-store).

### 6.1 Manifest header

Every manifest blob begins with:

```
offset  size  field
   0      4   magic                "TMFT"
   4      2   version              1
   6      1   manifest_kind        see §6.2
   7      1   level                tree depth, 0 = leaf
   8      8   logical_size         total bytes of represented content
  16      8   chunk_size_avg       average chunk size (informational; from CDC)
  24      4   entry_count          number of entries in the body
  28      4   reserved
  32    ...   body
```

### 6.2 Manifest kinds

| kind | name           | description |
| ---- | -------------- | --- |
|  1   | `INLINE`       | Body is the raw file content. `entry_count = 0`. `logical_size ≤ min_chunk` (§6.5; default 16 KiB). |
|  2   | `CHUNK_LIST`   | Body is `entry_count` chunk records, each describing one chunk of the represented bytes. `level = 0`. |
|  3   | `CHUNK_TREE`   | Body is `entry_count` child-manifest hashes; the represented bytes are the concatenation of the children's content, in order. `level >= 1`. Each child must have `level = parent.level - 1`. |
|  4   | `DIRECTORY`    | Body is `entry_count` directory records (see tessera-vfs.md §3). |
|  5   | `SYMLINK`      | Body is the symlink target string. `logical_size` is the target length. |
|  6   | `XATTR_STORE`  | Body is `entry_count` (xattr_name, value_blob_hash) pairs. Internal use only. |
|  7   | `GC_ROOT_LIST` | Body is `entry_count` GC-root records; see §15. Anchored at inode 1. |

### 6.3 Chunk records (kind = CHUNK_LIST)

48 bytes per entry:

```
offset  size  field
   0     32   chunk_hash           SHA-256 of the chunk's content blob
  32      8   logical_offset       byte offset within the file where this chunk starts
  40      4   uncompressed_size    chunk size in bytes
  44      4   flags                reserved
```

Entries are sorted by `logical_offset` ascending. The chunk content blob is itself stored in some pack.

### 6.4 Tree records (kind = CHUNK_TREE)

40 bytes per entry:

```
offset  size  field
   0     32   child_manifest_hash
  32      8   logical_offset       byte offset where the child's content starts
```

Each child manifest's `logical_size` covers a contiguous range; the parent's `logical_size = sum(child.logical_size)`. The fan-out cap is 256 entries per level (so a 4-level tree handles up to 256^4 = 4G chunks ≈ 256 PB at 64 KiB chunks).

### 6.5 Content-defined chunking

CDC parameters for v1:

- Algorithm: FastCDC with gear hash, polynomial `0x6e2cb31a36ec1d23`.
- Window size: 48 bytes.
- Average chunk size: configurable per-write-class, default 64 KiB.
- Min chunk: average / 4 (default 16 KiB).
- Max chunk: average * 4 (default 256 KiB).
- Boundary mask: lower bits of gear hash; mask width = log2(average) + 1.

A file ≤ `min_chunk` is stored as `INLINE` (no chunking). A file > `min_chunk` is chunked; if the chunk count is ≤ 256, manifest is `CHUNK_LIST`; otherwise `CHUNK_TREE` with appropriate level.

### 6.6 Manifest determinism

Given the same file content and the same CDC parameters, a Tessera-FS implementation must produce the same manifest hash. This is required for cross-host content addressing and dedup.

Determinism is also what makes dedup *observable* across trust boundaries; the confidentiality consequences and the per-domain policy that bounds them are specified in §20 (`salted` domains deliberately sacrifice this property).

## 7. Inodes

The inode is the only on-disk POSIX-shaped object that mutates. An inode record is exactly 144 bytes. Inode-table B+tree leaves use the standard B+tree node format (§10) with `key_size = 8` (inode_no), `value_size = 144` (inode record). Subtracting the 32-byte node header, each leaf holds at most ⌊(4096 − 32) / (8 + 144)⌋ = 26 inodes.

### 7.1 Inode record

```
offset  size  field
   0      4   inode_no               (denormalized; matches the table key)
   4      4   gen                    increments on every write of this inode
   8      4   mode                   POSIX mode bits (incl. type)
  12      4   reserved_a
  16      4   uid
  20      4   gid
  24      8   atime_ns               nanos since Unix epoch
  32      8   mtime_ns
  40      8   ctime_ns
  48      8   btime_ns               creation time
  56      8   size                   logical_size of the manifest
  64      4   nlink                  hard-link reference count
  68      4   flags                  see §7.2
  72     32   manifest_hash          current content (zero if empty/special)
 104     32   xattr_hash             hash of XATTR_STORE manifest (zero if none)
 136      8   reserved_b             zero in v1; reserved for per-inode key id, etc.
144
```

`mode` follows the POSIX `S_IFMT` convention (regular, directory, symlink, etc.). Special files (FIFOs, sockets, device nodes) are not supported in v1; their `mode` values are reserved and rejected at create time.

### 7.2 Inode flags

| bit | name              | meaning |
| --- | ----------------- | --- |
|  0  | `IMMUTABLE`       | inode-level chflags(2) immutable; mutations rejected at VFS layer. (Note: blob-level immutability is unconditional; this flag controls whether the *inode's manifest pointer* may be changed.) |
|  1  | `APPEND_ONLY`     | append-only; rewrites that don't extend rejected. |
|  2  | `NODUMP`          | informational |
|  3  | `OPAQUE`          | union-FS opaque marker (we don't unionfs, but VFS expects the bit) |
|  4  | `SUBVOL_ROOT`     | this directory inode is a subvolume root (§14). Snapshot/diff/send operations bound to subvolumes operate on the subtree rooted here. |
| 5-31 | reserved          | zero |

### 7.3 Inode table

The inode table is a B+tree (see §10) keyed by `inode_no` (u64). Leaves are 4 KiB blocks holding up to 26 (inode_no, inode_record) entries each. Internal nodes are 4 KiB blocks holding (key, child_block) pairs.

Inode `0` is reserved (null/invalid). Inode `1` is reserved for fsck/system internal use. Inode `2` is the root directory. Inode allocation begins at 3.

### 7.4 Inode allocation

- New inodes get the lowest unused number, tracked by `next_inode_no` in the superblock.
- `INODE_FREE` records mark a number reusable. Reuse is allowed; the `gen` field discriminates stale handles.
- Numbers are NOT recycled aggressively; the allocator may keep a free list and reuse only after a quiescent period to limit gen-collision risk in long-running open-fd handles.

## 8. Pack registry

The pack registry is a B+tree (§10) keyed by `pack_id` (UUID, 16 bytes). Leaves carry pack metadata; internal nodes carry routing.

### 8.1 Pack registry leaf entry

64 bytes per entry:

```
offset  size  field
   0     16   pack_id
  16      8   start_sector
  24      8   length_sectors
  32      4   blob_count
  36      4   pack_kind
  40      8   total_bytes
  48      8   create_time
  56      4   reachable_blobs        updated by GC; 0 if never scanned
  60      4   flags                  bit 0 = SEALED, bit 1 = RETIRING
```

Pack-registry mutations go through the journal as `PACK_PUBLISH` and `PACK_RETIRE` records.

### 8.2 Bloom-of-blooms

The registry maintains a top-level Bloom filter that is the union of every pack's bloom (computed lazily; an in-memory cache rebuilt at mount). A blob-hash lookup begins with this filter. If it says "definitely not present," the lookup terminates without consulting any pack.

This filter is never stored on disk in v1; rebuilding from per-pack blooms at mount is fast (O(packs) with each rebuild dominating the bloom size).

## 9. Free-extent map

The free-extent map covers the pack-file zone only and tracks runs of free blocks.

### 9.1 Layout

A B+tree (§10) keyed by `start_sector` (u64). Leaves carry `(start_sector, length_sectors)` extent records; internal nodes carry routing.

### 9.2 Extent record

16 bytes:

```
offset  size  field
   0      8   start_sector
   8      8   length_sectors
```

Adjacent extents are merged automatically on free. Extent allocation policy is best-fit with anti-fragmentation rotation (rotate among large extents to spread wear).

Allocations and frees go through the journal as `EXTENT_ALLOC` / `EXTENT_FREE` records.

## 10. B+tree node format

Used for the inode table, pack registry, and free-extent map. A unified node layout simplifies code.

### 10.1 Node header

Each node is one block (4 KiB).

```
offset  size  field
   0      4   magic                "TBTR"
   4      2   version              1
   6      1   node_kind            0 = leaf, 1 = internal
   7      1   tree_kind            0 = inode, 1 = pack-registry, 2 = free-extent
   8      4   entry_count
  12      4   key_size             bytes per key
  16      4   value_size           bytes per value (leaves only; internal nodes always 8-byte child sector)
  20      4   reserved
  24      4   crc32                CRC of bytes 0..(end of populated entries)
  28      4   reserved
  32    ...   entries: (key, value) pairs back-to-back
```

### 10.2 Leaf entries

`(key, value)` pairs of `(key_size + value_size)` bytes each, sorted ascending by key. For the inode-table case `key_size=8, value_size=128`. For pack-registry `key_size=16, value_size=64`. For free-extent `key_size=8, value_size=8`.

### 10.3 Internal entries

`(key, child_sector)` pairs of `(key_size + 8)` bytes. The key is the smallest key in the child subtree; lookup walks the largest key ≤ search_key.

### 10.4 COW semantics

Updates to any tree write a new leaf and propagate up, allocating new internal nodes for every changed path. The new root sector + generation is recorded with a `ROOT_UPDATE` record in the journal. Old blocks are freed at TX_COMMIT time via `EXTENT_FREE` records (against the free-extent map itself, which forms a dependency cycle resolved by reserving a small "free-of-freed" buffer area).

## 11. Garbage collection

GC reclaims pack-file blocks holding unreachable blobs.

### 11.1 Reachability

A blob hash is *reachable* if:
- It appears as an inode's `manifest_hash` for any live inode, or
- It appears as an inode's `xattr_hash` for any live inode, or
- It appears as a chunk hash within a reachable manifest's body, or
- It appears as a child-manifest hash within a reachable tree-manifest, or
- It is referenced from a directory manifest entry (the entry's child inode is alive AND its manifest is reachable), or
- It is a `pinned_hash` in the GC-root list (see §15) whose `expires_at_ns` has not passed.

GC computes this set by scanning live inodes and the GC-root list, then transitively walking manifests.

### 11.2 GC algorithm (pseudocode)

```
1. Snapshot the current generation; lock superblock writes briefly.
2. Walk inode table; for each live inode, push manifest_hash and xattr_hash into a working set.
3. Walk the working set; for each manifest blob, parse it and add child hashes (chunk or sub-manifest) to the set.
4. Iterate until the set stabilizes. Result: the live blob-hash set L.
5. For each pack p in the registry:
     count = |{h in p.index : h in L}|
     if count == p.blob_count: pack is fully live; skip.
     if count == 0: pack is fully dead; mark for retirement.
     else: pack is partially live; mark for repack.
6. For each marked-for-repack pack:
     - Allocate a new pack extent.
     - Copy live blobs from old pack to new pack.
     - Build new index + bloom.
     - Journal: PACK_PUBLISH(new) ; PACK_RETIRE(old) ; GC_TOMBSTONE(old).
     - Commit. Old pack's extent is now free.
7. Free fully-dead packs: PACK_RETIRE + EXTENT_FREE.
```

GC is incremental and idempotent; interruption at any step leaves the volume in a consistent state. Subsequent GC re-derives the set and continues.

### 11.3 GC concurrency

Writers run concurrently with GC. New writes during GC always reference reachable blobs (a writer never publishes an inode pointing at an unreachable blob); the steady-state invariant is preserved.

The window where a freshly-written blob exists in a pack but its referencing inode hasn't been committed yet is handled by *grace*: GC ignores packs whose `create_time` is within the last `gc_grace_seconds` (default 300). This prevents a race where GC sees a pack with a "dead" blob, but a transaction is in flight that would make it live.

## 12. Layout invariants

- The journal is sized at format time. Default: `max(64 MiB, 1% of volume)`.
- The inode-table zone is sized to hold `min(initial_size, 0.1% of volume)` worth of blocks. Beyond that, additional blocks are allocated from the free-extent zone.
- The pack-registry zone is sized for `1024` packs initial capacity; expansion uses the free-extent map.
- The pack-file zone is the rest of the volume.

## 13. mkfs.tessera contract

Format-time tool produces a Tessera-FS volume. Required steps:

1. Write zero blocks for sectors 0..4 (clears stale superblocks).
2. Compute zone offsets per the configured sizes.
3. Initialize an empty inode table with:
   - Inode 1 (GC-root anchor): `mode = S_IFREG | 0600`, `uid = gid = 0`, `manifest_hash` = empty `GC_ROOT_LIST` manifest hash, `nlink = 1`.
   - Inode 2 (root dir): `mode = S_IFDIR | 0755`, implicit subvolume root, `manifest_hash` = empty `DIRECTORY` manifest hash, `nlink = 1`.
4. Initialize an empty pack registry root.
5. Initialize a free-extent map with one entry covering the entire pack zone.
6. Write the empty-`DIRECTORY` and empty-`GC_ROOT_LIST` manifest blobs into a fresh pack.
7. Publish the pack via the registry (journal-free during mkfs; direct write).
8. Write SB_A and SB_B with `generation = 1`, `last_unmount_clean = 1`, `next_inode_no = 3`.
9. Write SB_A_backup and SB_B_backup at the volume tail.

A volume freshly mkfs'd contains: 2 inodes (GC-root anchor + root dir), 1 pack (containing 2 manifest blobs), and a free pool covering the rest.

## 14. Subvolumes

A subvolume is a directory subtree treated as an independently-versioned unit. Snapshot, send/receive, and diff operations may target a subvolume root rather than the whole volume.

### 14.1 Marking

An inode is a subvolume root iff its `flags` field has bit 4 (`SUBVOL_ROOT`) set. The root directory (inode 2) is implicitly a subvolume root regardless of the flag bit. Subvolume roots must be directory inodes; setting the flag on a non-directory inode is rejected.

### 14.2 Properties

A subvolume root scopes:
- **Snapshot identity.** A snapshot of `/foo` (a subvolume root) records the manifest hash of `/foo`'s directory tree, not the volume root's. Restoring is a single-inode `MANIFEST_REPOINT`.
- **Diff scope.** A diff from snapshot S1 of `/foo` to snapshot S2 of `/foo` walks only the manifests reachable from `/foo`, not the volume's full tree.
- **Send/receive scope.** Sending `/foo` produces a diff stream covering exactly the subvolume's reachable hashes.

A subvolume does **not** enforce access boundaries. Per-jail isolation is Portcullis's responsibility (mount-tree + capabilities); subvolumes are a structural primitive for snapshot and replication scope.

### 14.3 Nested subvolumes

A subvolume root contained within another subvolume root is permitted. A diff/send of the outer subvolume includes inner subvolume content as ordinary directory entries (the SUBVOL_ROOT flag is preserved on the inner inode but does not bound the outer's traversal). `tessera subvol send --no-recurse` skips inner subvolumes; default is recursive.

### 14.4 Lifecycle

- Creating a subvolume: allocate a directory inode, set `SUBVOL_ROOT`, link into parent. Atomic; one transaction.
- Deleting a subvolume: standard `rmdir` after emptying. The flag has no special semantics for deletion.
- Promoting an existing directory: requires the directory be empty (avoids retroactive snapshot-history confusion). Otherwise `EBUSY`.

## 15. GC roots

The GC-root list pins manifest hashes that may not yet have a referencing live inode but must remain reachable. Use cases: previous OS-version rollback targets, externally-staged transfers, user-marked "keep this around" snapshots.

### 15.1 Storage

Inode 1 is the **GC-root anchor**. Its `mode = S_IFREG | 0600`, `uid = 0`, `gid = 0`. Its `manifest_hash` points at a `GC_ROOT_LIST` manifest (kind = 7). Its `xattr_hash` is reserved (zero in v1).

### 15.2 GC_ROOT_LIST manifest body

The body is `entry_count` records, each variable-length:

```
offset   size   field
   0       4    record_length        total bytes (8-byte aligned)
   4       2    name_length          (1..127)
   6       2    flags                bit 0 = HAS_EXPIRY
                                     bit 1 = SIGNED  (signature follows; v2)
                                     bit 2 = SYSTEM  (managed by FS, e.g. recent-snapshot pin)
   8      32    pinned_hash          manifest hash to retain reachability for
  40       8    expires_at_ns        Unix nanos; ignored if HAS_EXPIRY clear
  48       8    created_at_ns        Unix nanos
  56     name   name                 UTF-8, no NUL, must be unique within the list
  ...    pad    zero-fill to 8-byte boundary
```

Records are sorted ascending by `name`. Maximum 16 384 entries per list (a hard cap keeps the manifest below 1 MiB; a deployment needing more pins should organize them under a separate inode or layer above).

### 15.3 GC behavior

In step 2 of the GC algorithm (§11.2), the working set is seeded with:
- All live inodes' `manifest_hash` and `xattr_hash`, AND
- Every `pinned_hash` in inode 1's GC_ROOT_LIST whose `expires_at_ns > now_ns` (or whose HAS_EXPIRY flag is clear).

Expired entries are pruned from the list as a side effect of the next GC mark phase: an `INODE_WRITE` repoints inode 1 to a new GC_ROOT_LIST manifest with expired records removed. This pruning is journaled.

### 15.4 Mutation operations

GC-root mutations follow the standard inode-write transaction shape:

1. Read current GC_ROOT_LIST manifest.
2. Build new manifest with the entry inserted/removed/updated.
3. Hash, store as new blob (or merge into open pack).
4. Transaction:
   - `PACK_PUBLISH` if a new blob was added.
   - `INODE_WRITE` updating inode 1's manifest_hash and bumping gen.
   - `MANIFEST_REPOINT` (inode 1).
   - `TX_COMMIT`.

Concurrent mutators serialize on a per-inode-1 lock taken at transaction begin.

### 15.5 Implicit roots maintained by the FS

The implementation MUST insert and maintain the following SYSTEM-flagged GC roots without user action:

- `__current_root` — pinned_hash equals the current root directory's manifest hash. Updated on every commit. Defends against a GC race where the in-memory root is stale.
- `__last_n_commits` — the last N (default 3) snapshots of inode 2's manifest hash, with HAS_EXPIRY flag set to a configurable retention window (default 7 days). Allows trivial rollback to recent states.
- `__open_xfers` — in-flight `tessera-receive` transfers add a temporary entry per transfer, removed on completion or expired automatically.

User pins use unique names without leading `__` to avoid collision with system-managed pins.

## 16. Diff stream format

A diff stream is the byte sequence describing "what differs between manifest A and manifest B" — used by `tessera-send`/`tessera-receive` for replication, by Opifex for incremental package distribution, and by backup tools.

### 16.1 Stream structure

```
+----------------------------+
|  Stream header (32 bytes)  |
+----------------------------+
|  HAVE bloom (variable)     |  receiver-known hashes; allows sender to skip sends
+----------------------------+
|  Body: sequence of records |
|  ...                       |
+----------------------------+
|  Stream footer (24 bytes)  |
+----------------------------+
```

### 16.2 Stream header

```
offset  size  field
   0      8   magic               "TESSDIFF"
   8      4   version             1
  12      4   flags               bit 0 = HAVE_BLOOM_PRESENT, bit 1 = COMPRESSED
  16     32   target_hash         the manifest the stream represents fully or as delta
  48     32   source_hash         delta basis (zero = full transfer, no basis)
  80      8   declared_blob_count  total live blobs in target
  88      8   declared_total_bytes bytes in target's reachable blobs
```

### 16.3 HAVE bloom (optional)

Receiver-side optimization. Before sending, the receiver may transmit a Bloom filter of hashes it already has in its CAS. The sender consults this filter to skip blobs the receiver definitely already has, emitting `HAS` records (informational) for those.

The HAVE bloom is a standard Bloom filter formatted identically to pack-file blooms (see §5.4). Its size and parameters are encoded in 32 bytes of preamble before the bit-array:

```
offset   size  field
   0       4   bloom_bytes
   4       4   bloom_hash_count
   8      24   reserved
  32   ...     bit array
```

If the HAVE_BLOOM_PRESENT flag is clear, this section is absent and the sender transmits all needed blobs unconditionally.

### 16.4 Body records

Every record has a 16-byte header:

```
offset   size  field
   0       1   record_kind
   1       3   reserved
   4       4   payload_length
   8       4   crc32_payload
  12       4   reserved
```

Record kinds:

| kind | name              | payload |
| ---- | ----------------- | --- |
|  1   | `HAS`             | 32 bytes — hash the receiver already has |
|  2   | `BLOB`            | 32 bytes hash + 4 bytes uncompressed_size + 4 bytes compressed_size + blob bytes |
|  3   | `MANIFEST`        | 32 bytes hash + 4 bytes manifest_size + manifest bytes (a manifest blob; recipient should walk its references) |
|  4   | `END_TARGET`      | 32 bytes target_hash; signals end of stream |

Note: the diff stream is a *blob transport*, not an inode-table replicator. Source-side inode numbers don't translate to the receiver (each volume has its own inode-number space). The receiver applies the new manifest hash to a target inode of *its* choosing — typically the destination subvolume root — as a separate operation after the stream is fully received.

### 16.5 Stream footer

```
offset  size  field
   0      8   magic               "TDIFFEND"
   8      4   record_count
  12      4   crc32_stream         CRC over header+body+footer (zero in this field at compute time)
  16      8   reserved
```

### 16.6 Send algorithm

Sender constructs the stream as follows:
1. Write header with `target_hash`, optional `source_hash`.
2. If receiver provided HAVE bloom, copy it into the stream.
3. Walk the reachable hash set of `target_hash` (the manifest's full transitive closure):
   - If `source_hash != 0`, exclude hashes also reachable from `source_hash` (incremental mode).
   - For each remaining hash, check the HAVE bloom; emit `HAS` for hits and skip; emit `BLOB` (or `MANIFEST` for manifest-kind blobs) otherwise.
4. Emit `END_TARGET`.
5. Emit footer with CRC.

### 16.7 Receive algorithm

Receiver:
1. Read and validate header. If `source_hash != 0`, verify it exists locally; otherwise abort.
2. Read body records sequentially:
   - `HAS`: verify the hash is locally present. If not, abort with INTEGRITY_ERROR.
   - `BLOB`/`MANIFEST`: verify the data hashes to the stated hash; store in CAS (in a new pack created for this transfer); on hash mismatch, abort.
   - `END_TARGET`: validate `target_hash` matches header.
3. Read and validate footer CRC. If mismatch, abort and roll back any new packs.
4. The receiver now has all blobs reachable from `target_hash` in its CAS. To make them visible in its filesystem, the user runs a separate `tessera subvol rollback <subvol> <target_hash>` (or equivalent) operation — that's a single inode-write transaction repointing a subvolume's manifest hash to `target_hash`.

### 16.8 Verification

Every received blob is hashed before storage. Hash mismatch is an immediate abort. After a successful receive, the receiver's CAS contains every reachable blob from `target_hash`; an explicit `tessera fsck --reachable target_hash` verifies the closure as a defensive check.

### 16.9 Use cases

- **Incremental backup**: `tessera-send -d <yesterday> /home/user > today.diff`. Apply with `tessera-receive < today.diff`.
- **Atomic OS update**: Opifex computes target manifest hash, requests diff stream from registry, applies via tessera-receive, then a single inode repoint promotes the new tree. Failed transfer leaves prior tree intact.
- **Volume migration**: `tessera-send /` from old machine, `tessera-receive` on new. Identical to backup-and-restore, just without an intermediate file.

## 17. Open questions for v2

These do not block v1 implementation but are tracked for future spec revisions:

- **Compression** of blob bytes (`compressed_size != 0` field is reserved). zstd-3 is the front-runner; per-pack-kind defaults likely.
- **Encryption** at the blob layer, with convergent encryption keyed by content hash (so dedup survives encryption). Caveat: convergent encryption preserves the dedup existence oracle by construction — see §20.3 for the constraints any v2 design must satisfy.
- **Snapshot/clone** API at the inode level: a snapshot is just a pinned root-directory manifest. The on-disk shape needs no change; the user-facing API requires VFS support.
- **Extended manifest types** for sparse files, holes, replication metadata.
- **Multi-device volumes** (RAID-Z-style striping inside Tessera). Out of scope for v1.

## 18. Worked example: writing a 17 KiB file

For grounding. Assume a fresh volume; user writes a 17 KiB regular file at `/example.txt`.

1. CDC chunks the 17 KiB. With default 64 KiB average / 16 KiB min, the file falls below `min_chunk` → no chunking. The file is stored as a single `INLINE` manifest:
   - Manifest body = the 17 KiB content.
   - Manifest size = 32 (header) + 17,408 = 17,440 bytes.
2. Manifest hash = SHA-256(manifest blob) → call it `M`.
3. Allocate a free extent for a new tiny-pack (call its sectors `p..p+P`).
4. Pack header: `blob_count=1`, `pack_kind=0`.
5. Pack index: one entry `(M, offset=data_offset, size=17440)`.
6. Pack bloom: trivial 1-bit set.
7. Pack data: `TBLB`-prefixed manifest bytes, 64-byte aligned.
8. Pack footer with CRC.
9. Allocate inode `3`. Build inode record: `mode=0644 reg`, `uid/gid=writer`, timestamps now, `size=17408`, `nlink=1`, `manifest_hash=M`, `xattr_hash=0`.
10. Read root directory's manifest, build a new directory manifest with the entry `("example.txt", 3)`. Hash it → `D`. The new directory manifest is a second blob to add to the same new pack.
11. Pack contains two blobs: `M` (the file content manifest) and `D` (the new directory manifest). Header, index, bloom, data area (with both blobs), footer all written and CRC'd. The pack is now sealed.
12. Journal transaction:
    - `TX_BEGIN` (tx_id 17, reason "write /example.txt")
    - `PACK_PUBLISH` (pack_id new, location p..p+P, blob_count=2, ...)
    - `INODE_WRITE` (inode 3, full record)
    - `MANIFEST_REPOINT` (inode 2, new_manifest_hash=D)
    - `EXTENT_ALLOC` (p..p+P)
    - `ROOT_UPDATE` (inode_root, new_root_sector, new_gen)
    - `TX_COMMIT` (tx_id 17)
13. fsync flushes the journal blocks. Superblock B is overwritten with the new inode-table root + pack-registry root, generation incremented. The transaction is now durable.

A second writer creating a file with byte-identical content would compute the same manifest hash `M`, find it already in the registry's bloom-of-blooms, look it up, get a hit, and reuse the existing blob — adding only a new directory entry and an inode pointing at `M`. Storage cost of the second instance: ~256 bytes (inode + dirent), not 17 KiB.

This is the property that makes the architecture worth the engineering.

## 19. Inspirations and references

This appendix lists prior work whose designs influenced Tessera-FS, with brief credit and pointers. Adoption is at the *idea* level; specific data structures and APIs in this spec are Tessera-FS's own.

| Source | Idea adopted |
| --- | --- |
| **Git** (Linus Torvalds, Junio Hamano) | Pack/idx file format, mark-sweep GC over reachability, multi-pack-index, reachability bitmaps. Tessera's pack-and-idx layout is a direct adaptation. |
| **OSTree** (Colin Walters et al., Red Hat) | Commit/ref split, atomic switch via root-pointer update, signed manifests. Influenced our manifest-as-snapshot model and the deploy/rollback shape. |
| **ZFS** (Jeff Bonwick et al., Sun) | COW-everything, ARC cache strategy, `send`/`receive` semantics, periodic scrub. Diff stream and scrub borrow directly from the ZFS playbook. |
| **bup** (Avery Pennarun) | FastCDC chunking parameters, proven CAS-with-CDC at TB scale. |
| **restic** (Alexander Neumann) | Pack format with per-pack bloom filters, encrypted convergent dedup. Validates our pack layout choices. |
| **borgbackup** (The Borg Collective) | Append-only repository design, deduplication-aware backup primitives. |
| **casync** (Lennart Poettering) | HTTP-deliverable content-addressed indexes; streaming download by chunk. Inspires future Opifex transport. |
| **Plan9 Venti** (Sean Quinlan, Sean Dorward) | Immutable block CAS as a layer beneath a mutable nameservice. The architectural split that defines Tessera. |
| **Plan9 Fossil** | Periodic snapshot of the writable layer into the immutable Venti store; influenced our snapshot-pin model. |
| **Nix** (Eelco Dolstra) | GC-root concept with explicit pinning beyond live-references; content-addressed immutable store with atomic switching. |
| **APFS** (Apple) | Instant clone semantics via copy-on-write metadata; informed our reflink (`vop_copy_file_range`) design. |
| **btrfs** (Chris Mason et al.) | Subvolume primitive; `send`/`receive` UX; `cp --reflink` exposure pattern. |
| **EROFS / SquashFS** | Tail-packing for sub-page blobs in compressed read-only FSes; informed pack-internal alignment choices and the small-blob storage tier. |
| **bcachefs** (Kent Overstreet) | Modern B-tree layout patterns, per-file metadata flexibility. |
| **FastCDC** (Wen Xia et al., HUST 2016) | The gear-hash content-defined chunking algorithm Tessera uses. |
| **Karythra-FS** (girivs) | The CAS-FS implementation in karythra-os from which Tessera's algorithmic pieces (dual-superblock atomicity, CRC32-validated metadata, write-session model, page cache) descend. |

Where Tessera diverges:
- Unlike Git/OSTree, Tessera presents a POSIX VFS (regular `read`/`write`/`stat`), not just an object store.
- Unlike ZFS/btrfs, Tessera dedup is content-keyed and cross-host portable; it isn't a within-volume optimization.
- Unlike Plan9 Fossil/Venti, Tessera runs on a normal POSIX OS and integrates with FreeBSD's VFS layer rather than presenting a 9P interface.
- Unlike Nix, Tessera operates at the file/chunk level, not the package/derivation level.

The combination — POSIX-shaped, content-keyed, cross-host portable, chunk-level dedup, integrated with capability-isolated jails — is what Tessera contributes that none of the references provide alone.

## 20. Security considerations — the dedup existence oracle

> Added 2026-06-10 after architecture review. Normative for v1.

### 20.1 The threat

Cross-domain dedup is observable, and observability is an
**existence oracle**: a writer who can detect whether its own
write was deduplicated learns that *someone else on the system
already stores those exact bytes*. This is the classic CAS/dedup
side channel (Harnik, Pinkas, Shulman-Peleg, *Side Channels in
Cloud Services: Deduplication in Cloud Storage*, IEEE S&P
Magazine 2010; the same family as KSM memory-dedup attacks).

CDC chunking makes confirmation strong: with a 16 KiB min chunk,
a low-entropy secret embedded in a known template (a config file
with a password field, a document with a known letterhead) is
brute-forceable candidate-by-candidate. Concrete attacks from an
unprivileged jail:

- "Is app X installed?" — probe chunks of its binary.
- "Does any user possess this exact document / version?"
- Dictionary attack on a secret inside a known file shape.

The observation channels, strongest first:

1. **Physical free space** (`statfs`). Write a candidate, fsync,
   re-read free space. Unchanged → dedup hit. Noise-free.
   Closed by quota-scoped statfs (tessera-quotas.md §3.6) **on a
   mount carrying a whole-FS quota**. ⚠ That closure does NOT
   currently reach a Portcullis jail: jails share one volume
   (portcullis.md §4.1), statfs scoping is per-mount not per-path,
   and a jail's `df` is answered by unionfs, which passes the
   underlying pool's free space through (measured 2026-08-04).
   See tessera-quotas.md §3.6.2 — OPEN.
   ⚠⚠ And §20.2's `deferred` policy does NOT cover for it: the
   policy field is set but never read by the kmod, so every write
   dedups synchronously. Measured 4 MiB duplicate = 5 blocks vs
   4 MiB novel = 1039 blocks, three rounds, zero variance — the
   oracle is open and noise-free. Task #114.
2. **Write/fsync timing.** A synchronous-dedup hit skips the
   pack append; latency is content-dependent. Closed by the
   `deferred` policy below.
3. **Volume-wide counters** (`tessera stat` dedup ratio, blob
   count; quota sysctls). Closed by restricting these to the
   host (tessera-vfs.md §13.7, tessera-quotas.md §6.2).

Note what is *not* threatened: content confidentiality. A hash
never grants access to bytes (aqueduct.md §6.2); the oracle
leaks one bit — existence — per probe. That bit is enough to
matter for Atrium's stated audiences (secure workstations,
license-strict orgs, journalists' sources).

### 20.2 Design: dedup domains

The resolution rests on one observation: **the disk-cost thesis
("N jailed apps ≈ 1× storage") is won entirely on trusted-ingest
content** — app trees written by atrium-pkg / tessera-import,
where the content is public-ish (binaries, libraries, assets)
and global synchronous dedup is both safe and maximally
valuable. Per-jail overlays — where untrusted writers put user
data, i.e. where the secrets live — contribute marginal
cross-jail dedup in legitimate use.

So dedup becomes a **per-domain policy**, not a universal CAS
property. A *dedup domain* is a directory tree with one of three
policies (the domain boundary deliberately coincides with the
quota-domain boundary, tessera-quotas.md §4.2 — one record, one
tree-attachment mechanism):

| Policy | Write path | Cross-domain dedup | Oracle exposure | Intended use |
|---|---|---|---|---|
| `global` | synchronous: registry hit skips the append | total, immediate | full (mitigated only by statfs/counter scoping) | trusted-ingest trees: `/var/lib/atrium/apps/`, OS images. Writers are rank ≥3 (atrium-pkg, tessera-import). |
| `deferred` (default for overlays) | **append-anyway**: blob bytes are written unconditionally; the pack-registry insert that finds an existing entry for the hash keeps the existing entry and marks the new extent dead | total *at rest*, converged by the next repack pass (§11, tessera-vfs.md §13.6) | timing + free-space deltas are content-independent at observation time; residual channel is repack-cadence-granular and noisy | per-jail overlays, user home volumes |
| `salted` | chunk hash is `SHA-256(domain_salt ‖ content)`; 32-byte salt drawn at domain creation, stored in the QuotaDomain record | none (by construction) | none | opt-in high-sensitivity volumes (`privacy = true` in the volume manifest) |

Notes:

- `deferred` requires **no format change**: duplicate appends are
  ordinary blobs whose registry insert loses the race; the dead
  extent is reclaimed by the existing repack machinery. Manifests
  reference content by hash, so convergence never rewrites a
  manifest. The cost is transient double storage bounded by
  repack cadence.
- `deferred` write paths MUST NOT short-circuit on a registry
  hit, MUST NOT skip journal/pack I/O on a hit, and SHOULD avoid
  any hit-dependent branch with measurable latency before the
  fsync completes. The registry consultation itself can move
  entirely to repack time.
- `salted` breaks cross-host content addressing and diff-stream
  dedup (§16) for that domain — stated and intended. The HAVE
  bloom (§16.3) of a salted domain reveals nothing about other
  domains.
- Snapshots inherit the domain policy of the tree they pin.

### 20.3 Interaction with future encryption (v2)

Convergent encryption (§17) keyed purely by content hash has
exactly this confirmation-of-content weakness as its *known
fundamental property* — it would reintroduce the oracle at the
crypto layer. Any v2 encryption design MUST compose the domain
salt into the key derivation for `salted` domains, and SHOULD
treat "convergent across domains" as a per-domain opt-in
equivalent to `global`.

### 20.4 What v1 explicitly accepts

- `global`-domain probing remains possible for anyone who can
  write into a `global` domain. Writers there are trusted system
  components by construction; the policy file that grants a jail
  write access to a `global` domain is the security statement.
- Repack-cadence inference against `deferred` domains (watch
  global free space recover after repack). Coarse, noisy,
  host-schedulable; documented residual risk.
- Page-cache sharing of legitimately shared files (standard VM
  behavior on every OS; not a CAS property).
