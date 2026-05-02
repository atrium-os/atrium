/*
 * tessera/format.h — on-disk struct definitions.
 *
 * Mirrors tessera-fs.md exactly. Field layout is normative; do not
 * reorder, resize, or pad without bumping the format version. All
 * multi-byte integers are little-endian.
 *
 * Structs marked TESSERA_PACKED have no implicit padding and are
 * compiled with __attribute__((packed)) (or #pragma pack equivalent).
 * Static_asserts at the end of this file verify their sizes match
 * the spec.
 */

#ifndef TESSERA_FORMAT_H_
#define TESSERA_FORMAT_H_

#ifdef _KERNEL
#  include <sys/types.h>
#else
#  include <stdint.h>
#  include <stddef.h>
#endif

#ifdef __cplusplus
extern "C" {
#endif

#if defined(__GNUC__) || defined(__clang__)
#  define TESSERA_PACKED __attribute__((packed))
#else
#  error "tessera-core requires a compiler with __attribute__((packed))"
#endif

/* ── Constants ───────────────────────────────────────────────────── */

#define TESSERA_SECTOR_SIZE          4096u
#define TESSERA_HASH_SIZE              32u    /* SHA-256 = 32 bytes */
#define TESSERA_PATH_NAME_MAX         255u    /* per dir entry */
#define TESSERA_XATTR_INLINE_MAX     4096u
#define TESSERA_XATTR_NAME_MAX        255u
#define TESSERA_TREE_FANOUT_MAX       256u    /* manifest tree */
#define TESSERA_PIN_NAME_MAX          127u    /* GC-root name */
#define TESSERA_INODE_RECORD_SIZE     144u
#define TESSERA_PACK_INDEX_ENTRY_SIZE  48u
#define TESSERA_REGISTRY_ENTRY_SIZE    64u
#define TESSERA_EXTENT_ENTRY_SIZE      16u

#define TESSERA_INODE_NULL              0u
#define TESSERA_INODE_GC_ROOT_ANCHOR    1u
#define TESSERA_INODE_ROOT_DIR          2u
#define TESSERA_INODE_FIRST_USER        3u

/* ── Hash ────────────────────────────────────────────────────────── */

typedef uint8_t tessera_hash_t[TESSERA_HASH_SIZE];

/* ── Magic strings ───────────────────────────────────────────────── */

#define TESSERA_MAGIC_SUPERBLOCK   "TESSERA1"
#define TESSERA_MAGIC_JOURNAL      "TJOURNAL"
#define TESSERA_MAGIC_TXR          "TXR\0"
#define TESSERA_MAGIC_PACK         "TPACK\0\0\0"
#define TESSERA_MAGIC_PACK_END     "TPACKEND"
#define TESSERA_MAGIC_BLOB         "TBLB"
#define TESSERA_MAGIC_MANIFEST     "TMFT"
#define TESSERA_MAGIC_BTREE_NODE   "TBTR"
#define TESSERA_MAGIC_DIFF         "TESSDIFF"
#define TESSERA_MAGIC_DIFF_END     "TDIFFEND"

/* ── Encryption key slot (256 bytes, embedded in SB) ──────────────
 *
 * Each slot is one independent way to recover the volume key. The
 * SB holds 8 slots; any unlock method that succeeds yields the
 * same 32-byte volume key, which then drives AES-XTS encryption of
 * every on-disk sector.
 *
 * Slot types (parallel to LUKS2 / FileVault / BitLocker):
 *   1 = passphrase  (KDF over user passphrase → KEK → unwrap)
 *   2 = TPM2        (TPM unseals an object with policy_digest →
 *                    KEK → unwrap; PCR-bound for measured boot)
 *   3 = recovery    (KDF over a long random string printed at
 *                    format time; for "I lost my password")
 *   4 = FIDO2       (hmac-secret extension on a registered
 *                    credential → KEK → unwrap)
 *   5 = PKCS#11     (smart-card / HSM)
 *   6 = keyfile     (raw bytes from a file on disk / USB stick)
 *
 * All slots share the same wrap format: XChaCha20-Poly1305 over
 * the 32-byte volume key, layout = nonce(24) + ct(32) + tag(16) +
 * reserved(8) = 80 bytes.
 */
typedef struct TESSERA_PACKED {
	uint8_t   slot_type;                 /* see enum above; 0 = unused */
	uint8_t   kdf_algorithm;             /* 0=none, 1=Argon2id, 2=PBKDF2-SHA256 */
	uint16_t  flags;                     /* bit 0 = primary slot */
	uint32_t  kdf_iterations;
	uint8_t   kdf_salt[16];
	uint8_t   wrapped_key[80];           /* XChaCha20-Poly1305 wrapped vol key */
	uint8_t   tpm_pcr_mask[8];           /* TPM2 slots: which PCRs to bind */
	uint8_t   tpm_policy_digest[32];     /* TPM2 slots: expected policy hash */
	uint8_t   credential_id[16];         /* FIDO2 / PKCS#11 ref */
	uint8_t   reserved[96];              /* future slot types */
} tessera_key_slot_t;

/* ── Superblock (4096 bytes) ─────────────────────────────────────── */

typedef struct TESSERA_PACKED {
	uint8_t   magic[8];                  /* "TESSERA1" */
	uint32_t  version_major;             /* 1 */
	uint32_t  version_minor;
	uint32_t  feature_flags;
	uint32_t  incompat_flags;
	uint64_t  generation;
	uint8_t   volume_uuid[16];
	uint64_t  total_sectors;
	uint32_t  sector_size;               /* 4096 */
	uint32_t  reserved_a;
	uint64_t  journal_start;
	uint64_t  journal_length;
	uint64_t  inode_root;
	uint64_t  inode_root_generation;
	uint64_t  pack_registry_root;
	uint64_t  pack_registry_gen;
	uint64_t  free_extent_root;
	uint64_t  free_extent_gen;
	uint64_t  pack_zone_start;
	uint64_t  pack_zone_length;
	uint64_t  next_inode_no;
	uint64_t  total_blob_count;
	uint64_t  total_pack_count;
	uint64_t  format_time;
	uint64_t  last_mount_time;
	uint64_t  meta_reserve_start;        /* sector of metadata reserve   */
	uint64_t  meta_reserve_length;       /* sectors                       */
	uint64_t  meta_reserve_bump;         /* next free sector in reserve  */
	/* Snapshots tree — reserved by v1 for v2's time-machine feature.
	 * v1 mkfs initialises snapshots_root=0 (uninitialised); v1 kmod
	 * never writes either field. v2 will allocate the tree and
	 * have commit_sb append a snapshot record per commit. Wiring
	 * the format slot now avoids an on-disk migration when v2 lands. */
	uint64_t  snapshots_root;
	uint64_t  snapshots_gen;
	/* Encryption — reserved by v1 for v3's at-rest encryption.
	 * v1 mkfs zeros all of these; v1 kmod never reads them.
	 * v3 will populate `key_slots` with active unlock methods
	 * (passphrase, TPM2-sealed, recovery key, FIDO2, etc.) and
	 * use the volume key — recovered by unwrapping any one slot —
	 * to AES-XTS-encrypt all on-disk sectors. Wiring the format
	 * slots now avoids an on-disk migration when v3 lands. */
	uint16_t  encryption_flags;          /* bit 0 = AES-XTS active
	                                        bit 1 = convergent (opt-in) */
	uint8_t   active_slot_count;         /* number of populated slots */
	uint8_t   reserved_e0;
	uint32_t  reserved_e1;
	uint8_t   master_key_id[16];         /* unique per-volume; key-rotation
	                                        tooling uses this to detect a
	                                        re-encryption */
	tessera_key_slot_t  key_slots[8];    /* 8 × 256 = 2 KiB */
	uint32_t  last_unmount_clean;
	uint32_t  crc32;                     /* CRC over bytes 0..(crc32 offset) */
	/* Keyed integrity — reserved by v1 for v3's authenticated metadata.
	 * v1 mkfs zeros this; v1/v2 kmod ignore it. v3 derives a separate
	 * mac_key = HKDF(volume_key, "tessera-mac") and writes
	 * HMAC-SHA256(mac_key, sb_bytes_with_hmac_zeroed) here on every
	 * commit_sb, verifying on mount. CRC32 stays for non-authenticated
	 * (encryption-off) volumes; HMAC supersedes it when present. */
	uint8_t   hmac[32];
	uint8_t   reserved[1760];
} tessera_superblock_t;

/* Superblock feature flags */
#define TESSERA_FEATURE_ENCRYPTED       (1u << 0)
#define TESSERA_FEATURE_MULTI_LEVEL_MFT (1u << 1)
#define TESSERA_FEATURE_BLOOM_V2        (1u << 2)

/* ── Journal header (4096 bytes) ─────────────────────────────────── */

typedef struct TESSERA_PACKED {
	uint8_t   magic[8];                  /* "TJOURNAL" */
	uint32_t  version;
	uint32_t  reserved_a;
	uint64_t  head_seq;
	uint64_t  tail_seq;
	uint64_t  head_block;
	uint64_t  tail_block;
	uint32_t  crc32;                     /* CRC over bytes 0..48 */
	uint8_t   reserved[4044];
} tessera_journal_header_t;

/* ── Journal record header (64 bytes) ──────────────────────────────
 *
 * Bytes 0..28 are the original header content (CRC covers them).
 * Bytes 32..63 are the v3-reserved HMAC slot — v1/v2 zero it; v3
 * fills with HMAC-SHA256(mac_key, header_bytes_with_hmac_zeroed)
 * (covers the same bytes as crc32_header plus the body bytes via
 * crc32_body). reserved_pad keeps the HMAC field 8-byte-aligned and
 * leaves room for a future per-record key-id. */
typedef struct TESSERA_PACKED {
	uint8_t   magic[4];                  /* "TXR\0" */
	uint32_t  record_type;
	uint64_t  sequence;
	uint32_t  body_length;
	uint32_t  block_count;
	uint32_t  crc32_body;
	uint32_t  crc32_header;
	uint8_t   hmac[32];
} tessera_record_header_t;

/* Journal record types (tessera-fs §4.4) */
typedef enum {
	TESSERA_TX_BEGIN          = 1,
	TESSERA_TX_COMMIT         = 2,
	TESSERA_TX_ABORT          = 3,
	TESSERA_INODE_WRITE       = 4,
	TESSERA_INODE_FREE        = 5,
	TESSERA_MANIFEST_REPOINT  = 6,
	TESSERA_DIR_INSERT        = 7,
	TESSERA_DIR_REMOVE        = 8,
	TESSERA_PACK_PUBLISH      = 9,
	TESSERA_PACK_RETIRE       = 10,
	TESSERA_EXTENT_ALLOC      = 11,
	TESSERA_EXTENT_FREE       = 12,
	TESSERA_ROOT_UPDATE       = 13,
	TESSERA_GC_TOMBSTONE      = 14,
} tessera_record_type_t;

/* DIR_INSERT / DIR_REMOVE body — variable-length, name bytes follow.
 * Used by the v2.6 dirent log to persist pending dirent ops to the
 * journal between commit_sb flushes. On replay, records re-create
 * the in-memory log so the first post-mount flush re-applies them
 * to the BTREE. */
typedef struct TESSERA_PACKED {
	uint32_t  parent_inode_no;
	uint32_t  inode_no;
	uint16_t  name_len;
	uint8_t   reserved[2];
	/* name_len bytes of name follow inline */
} tessera_jrec_dirent_t;

/* INODE_WRITE body — fixed 8-byte header + 144-byte inode_record.
 * Phase B.2 part 2: journals the inode body itself so a crash
 * before commit_sb leaves a complete (dirent + inode) replay set.
 * Without this, replayed dirents would reference inode_nos whose
 * body lives only in dirty_inodes and is lost on power-cut. */
typedef struct TESSERA_PACKED {
	uint32_t  inode_no;
	uint8_t   tombstone;       /* 1 = delete, 0 = put */
	uint8_t   reserved[3];
	/* tessera_inode_record_t body follows (144 bytes). Total = 152. */
} tessera_jrec_inode_t;

/* ── Pack header (4096 bytes) ────────────────────────────────────── */

typedef struct TESSERA_PACKED {
	uint8_t   magic[8];                  /* "TPACK\0\0\0" */
	uint32_t  version;
	uint32_t  pack_kind;                 /* 0=tiny, 1=small, 2=mixed */
	uint8_t   pack_id[16];               /* UUIDv4 */
	uint64_t  create_time;
	uint64_t  creator_tx_id;
	uint32_t  blob_count;
	uint32_t  index_blocks;
	uint32_t  bloom_bytes;
	uint32_t  bloom_hash_count;
	uint64_t  data_offset;
	uint64_t  data_length;
	uint64_t  total_pack_bytes;
	uint32_t  crc32_header;
	/* Keyed integrity — see SB hmac comment. v3 fills with
	 * HMAC-SHA256(mac_key, pack_header_bytes_with_hmac_zeroed). */
	uint8_t   hmac[32];
	uint8_t   reserved[3972];
} tessera_pack_header_t;

/* ── Pack index entry (48 bytes) ─────────────────────────────────── */

typedef struct TESSERA_PACKED {
	tessera_hash_t  blob_hash;
	uint64_t        data_offset;
	uint32_t        data_size;
	uint32_t        flags;               /* see below */
} tessera_pack_index_entry_t;

#define TESSERA_BLOB_FLAG_MANIFEST  (1u << 0)
#define TESSERA_BLOB_FLAG_CHUNK     (1u << 1)
#define TESSERA_BLOB_FLAG_INLINE    (1u << 2)

/* ── Blob descriptor (16 bytes; precedes blob bytes in pack data) ── */

typedef struct TESSERA_PACKED {
	uint8_t   magic[4];                  /* "TBLB" */
	uint32_t  uncompressed_size;
	uint32_t  compressed_size;           /* 0 = not compressed */
	uint32_t  reserved;
} tessera_blob_descriptor_t;

/* ── Pack footer (4096 bytes) ────────────────────────────────────── */

typedef struct TESSERA_PACKED {
	uint8_t   magic[8];                  /* "TPACKEND" */
	uint32_t  blob_count_check;
	uint32_t  crc32_pack;
	uint8_t   reserved[4080];
} tessera_pack_footer_t;

/* ── Manifest header (32 bytes) ──────────────────────────────────── */

typedef struct TESSERA_PACKED {
	uint8_t   magic[4];                  /* "TMFT" */
	uint16_t  version;
	uint8_t   manifest_kind;
	uint8_t   level;
	uint64_t  logical_size;
	uint64_t  chunk_size_avg;
	uint32_t  entry_count;
	uint32_t  reserved;
} tessera_manifest_header_t;

typedef enum {
	TESSERA_MFT_INLINE        = 1,
	TESSERA_MFT_CHUNK_LIST    = 2,
	TESSERA_MFT_CHUNK_TREE    = 3,
	TESSERA_MFT_DIRECTORY     = 4,
	TESSERA_MFT_SYMLINK       = 5,
	TESSERA_MFT_XATTR_STORE   = 6,
	TESSERA_MFT_GC_ROOT_LIST  = 7,
	/* v2 (added 2026-04-30): two-level DIRECTORY for huge dirs.
	 * Body holds tessera_dir_bucket_record_t entries pointing at
	 * inner DIRECTORY manifests, each holding a hash-range slice of
	 * the entries. Promoted from flat DIRECTORY when body crosses
	 * a threshold (~4 KiB). Lookup: hash(name) → binary-search
	 * outer for largest first_hash ≤ hash → descend into bucket. */
	TESSERA_MFT_DIRECTORY_2L  = 8,
	/* v2.5 (added 2026-05-02): content-addressed B+tree directory.
	 * Each node is its own manifest blob. Inner nodes hold sorted
	 * (max_name_hash, child_node_hash) records; leaf nodes hold
	 * sorted (name_hash, name, inode_no) records. The dir's
	 * manifest_hash points at the root node. Mutation = COW path
	 * of O(log_F N) nodes; lookup = O(log_F N) descent. F = fanout
	 * (~32 leaf entries / ~64 inner entries per node).
	 *
	 * Replaces DIRECTORY_2L for new mutations. The 2L code paths
	 * stay for read compatibility with older volumes; the first
	 * mutation on a 2L parent walks all entries and rebuilds as
	 * BTREE. */
	TESSERA_MFT_DIRECTORY_BTREE = 9,
} tessera_manifest_kind_t;

/* DIRECTORY_BTREE node header — first 8 bytes after the manifest's
 * 32-byte common header. Body is `count` records following the
 * header. Records are 40 B each for inner, variable for leaf:
 *
 *   inner record (40 B): u64 max_name_hash, tessera_hash_t child_hash
 *   leaf  record (var):  u64 name_hash, u16 name_len, name bytes,
 *                        u64 inode_no
 */
typedef struct TESSERA_PACKED {
	uint8_t   leaf_flag;     /* 0 = inner, 1 = leaf */
	uint8_t   reserved[3];
	uint32_t  count;
} tessera_dir_btree_node_header_t;

#define TESSERA_DIR_BTREE_FANOUT_LEAF   32u   /* split when leaf > this */
#define TESSERA_DIR_BTREE_FANOUT_INNER  64u   /* split when inner > this */

/* ── Directory bucket record (40 bytes/entry, kind=DIRECTORY_2L) ── */

typedef struct TESSERA_PACKED {
	uint64_t        first_name_hash;     /* smallest dir_name_hash in bucket */
	tessera_hash_t  bucket_manifest_hash;/* inner flat DIRECTORY manifest */
} tessera_dir_bucket_record_t;

/* ── Chunk record (kind = CHUNK_LIST, 48 bytes/entry) ───────────── */

typedef struct TESSERA_PACKED {
	tessera_hash_t  chunk_hash;
	uint64_t        logical_offset;
	uint32_t        uncompressed_size;
	uint32_t        flags;               /* bit 2 = ZERO_HOLE */
} tessera_chunk_record_t;

#define TESSERA_CHUNK_FLAG_ZERO_HOLE  (1u << 2)

/* ── Tree record (kind = CHUNK_TREE, 40 bytes/entry) ────────────── */

typedef struct TESSERA_PACKED {
	tessera_hash_t  child_manifest_hash;
	uint64_t        logical_offset;
} tessera_tree_record_t;

/* ── Inode record (144 bytes) ────────────────────────────────────── */

typedef struct TESSERA_PACKED {
	uint32_t  inode_no;                  /* denormalized; same as table key */
	uint32_t  gen;                       /* increments per write */
	uint32_t  mode;                      /* POSIX mode incl. S_IFMT */
	uint32_t  reserved_a;
	uint32_t  uid;
	uint32_t  gid;
	uint64_t  atime_ns;
	uint64_t  mtime_ns;
	uint64_t  ctime_ns;
	uint64_t  btime_ns;
	uint64_t  size;
	uint32_t  nlink;
	uint32_t  flags;                     /* see below */
	tessera_hash_t  manifest_hash;
	tessera_hash_t  xattr_hash;
	uint8_t   reserved_b[8];             /* future: per-inode key id, etc. */
} tessera_inode_record_t;

/* Inode flags (tessera-fs §7.2) */
#define TESSERA_INODE_FLAG_IMMUTABLE   (1u << 0)
#define TESSERA_INODE_FLAG_APPEND_ONLY (1u << 1)
#define TESSERA_INODE_FLAG_NODUMP      (1u << 2)
#define TESSERA_INODE_FLAG_OPAQUE      (1u << 3)
#define TESSERA_INODE_FLAG_SUBVOL_ROOT (1u << 4)

/* ── B+tree node header (64 bytes) ─────────────────────────────────
 *
 * Bytes 0..24 are the original CRC-covered header. Bytes 32..63
 * are the v3-reserved HMAC slot (zeroed by v1/v2). Adding the slot
 * inline keeps the auth tag co-located with the bytes it
 * authenticates and avoids a sidecar tree on the metadata hot path.
 * Cost is negligible: only the free-extent tree loses 2 entries
 * per 4 KiB block (254→252); inode and pack-registry trees keep
 * the same fanout. */
typedef struct TESSERA_PACKED {
	uint8_t   magic[4];                  /* "TBTR" */
	uint16_t  version;
	uint8_t   node_kind;                 /* 0 = leaf, 1 = internal */
	uint8_t   tree_kind;                 /* 0 = inode, 1 = pack-reg, 2 = free-ext */
	uint32_t  entry_count;
	uint32_t  key_size;
	uint32_t  value_size;
	uint32_t  reserved_a;
	uint32_t  crc32;
	uint32_t  reserved_b;
	uint8_t   hmac[32];
} tessera_btree_node_header_t;

/* ── Pack registry entry (64 bytes) ──────────────────────────────── */

typedef struct TESSERA_PACKED {
	uint8_t   pack_id[16];
	uint64_t  start_sector;
	uint64_t  length_sectors;
	uint32_t  blob_count;
	uint32_t  pack_kind;
	uint64_t  total_bytes;
	uint64_t  create_time;
	uint32_t  reachable_blobs;           /* 0 = never scanned */
	uint32_t  flags;                     /* SEALED, RETIRING */
} tessera_registry_entry_t;

#define TESSERA_REGISTRY_FLAG_SEALED        (1u << 0)
#define TESSERA_REGISTRY_FLAG_RETIRING      (1u << 1)
/* MULTI_EXTENT — pack body spans multiple non-contiguous data-zone
 * extents (ZFS-style "gang" fallback when the data zone is too
 * fragmented to allocate the pack contiguously).
 *
 *   set:    start_sector points at a 1-sector "pack extent list"
 *           (PEL, see tessera_pack_extent_list_t below) that holds
 *           the actual extent vector. length_sectors is the sum of
 *           the extents' lengths (= the logical pack body length).
 *   clear:  (start_sector, length_sectors) describe one contiguous
 *           extent containing the whole pack — fast path, identical
 *           to the v1/v2 layout. */
#define TESSERA_REGISTRY_FLAG_MULTI_EXTENT  (1u << 2)

/* Pack extent list (PEL) — one sector. Lives in the data zone,
 * allocated whenever a pack publish falls back to multi-extent.
 * Reader path reads this sector first to discover the extent vector,
 * then issues bread per extent.
 *
 * Layout: 4096 bytes. 32-byte header + extents[N] + crc32 at the
 * end. With 16-byte extent records, N_MAX = (4096 - 32 - 4) / 16
 *                                         = 254 extents.
 *
 * Per pack overhead: one extra sector (4 KiB) when multi-extent is
 * triggered. Fast path (single contiguous extent) pays nothing —
 * the flag bit drives the lookup and PEL is never allocated. */
typedef struct TESSERA_PACKED {
	uint64_t  start_sector;
	uint64_t  length_sectors;
} tessera_pack_extent_t;

#define TESSERA_PEL_MAGIC                   ((uint64_t)0x315056454C455054ULL) /* "TPELEV01" */
#define TESSERA_PEL_MAX_EXTENTS             253u

typedef struct TESSERA_PACKED {
	uint64_t              magic;        /* TESSERA_PEL_MAGIC */
	uint32_t              version;      /* 1 */
	uint32_t              extent_count; /* extents in THIS PEL only */
	uint64_t              total_length; /* sum across the entire CHAIN
	                                     * (only meaningful in the head
	                                     * PEL — continuation PELs may
	                                     * leave it 0). */
	/* PEL chaining: when one PEL can't hold all the extents for a
	 * pack (severe data-zone fragmentation; the cap is 253 extents
	 * per sector), the writer allocates a continuation PEL and links
	 * it here. Reader walks the chain until next_pel_sector == 0.
	 *
	 * Format-compat: pre-chaining writers always wrote 0 here (the
	 * field was named `reserved`); pre-chaining readers always
	 * ignored it. So old volumes mounted by new readers see 0 and
	 * stop after one PEL — same behaviour as before. New volumes
	 * mounted by old readers would be silently truncated, which is
	 * why we never wrote a non-zero value historically. */
	uint64_t              next_pel_sector;
	tessera_pack_extent_t extents[TESSERA_PEL_MAX_EXTENTS];
	uint8_t               pad[12];
	uint32_t              crc32;        /* CRC over bytes 0..(crc32 offset) */
} tessera_pack_extent_list_t;

/* ── Snapshot record (64 bytes) ─────────────────────────────────────
 *
 * One per commit_sb when the snapshots feature is active (v2). The
 * b+tree key is the 8-byte big-endian generation; the value is this
 * record. Every commit appends one. Retention pruning (log-decay,
 * pressure-based) runs from a separate v2 helper. Held-snapshots
 * provide GC anchoring (their roots' manifest hashes union into the
 * live set) so older history isn't reclaimed.
 *
 * `reason_tag` is human-readable: "auto", "user", "rollback", etc.
 */
typedef struct TESSERA_PACKED {
	uint64_t  generation;
	uint64_t  timestamp_ns;
	uint64_t  inode_root;
	uint64_t  pack_registry_root;
	uint64_t  free_extent_root;
	uint8_t   reason_tag[16];
	uint8_t   reserved[8];
} tessera_snapshot_record_t;

#define TESSERA_SNAPSHOT_RECORD_SIZE  64u

/* ── Free-extent record (16 bytes) ───────────────────────────────── */

typedef struct TESSERA_PACKED {
	uint64_t  start_sector;
	uint64_t  length_sectors;
} tessera_free_extent_t;

/* ── Static size assertions ──────────────────────────────────────── */

#define TESSERA_STATIC_ASSERT(expr, name) \
	typedef char tessera_static_assert_##name[(expr) ? 1 : -1]

TESSERA_STATIC_ASSERT(sizeof(tessera_key_slot_t)          == 256,  key_slot_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_superblock_t)        == 4096, superblock_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_journal_header_t)    == 4096, journal_header_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_record_header_t)     == 64,   record_header_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_pack_header_t)       == 4096, pack_header_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_pack_index_entry_t)  == 48,   pack_index_entry_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_blob_descriptor_t)   == 16,   blob_descriptor_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_pack_footer_t)       == 4096, pack_footer_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_manifest_header_t)   == 32,   manifest_header_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_chunk_record_t)      == 48,   chunk_record_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_tree_record_t)       == 40,   tree_record_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_inode_record_t)      == 144,  inode_record_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_btree_node_header_t) == 64,   btree_node_header_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_registry_entry_t)    == 64,   registry_entry_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_free_extent_t)       == 16,   free_extent_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_snapshot_record_t)   == 64,   snapshot_record_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_dir_bucket_record_t) == 40,   dir_bucket_record_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_pack_extent_t)       == 16,   pack_extent_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_pack_extent_list_t)  == 4096, pack_extent_list_size);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_FORMAT_H_ */
