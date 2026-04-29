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
	uint32_t  last_unmount_clean;
	uint32_t  crc32;                     /* CRC over bytes 0..188 */
	uint8_t   reserved[3904];
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

/* ── Journal record header (32 bytes) ────────────────────────────── */

typedef struct TESSERA_PACKED {
	uint8_t   magic[4];                  /* "TXR\0" */
	uint32_t  record_type;
	uint64_t  sequence;
	uint32_t  body_length;
	uint32_t  block_count;
	uint32_t  crc32_body;
	uint32_t  crc32_header;
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
	uint8_t   reserved[4004];
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
	TESSERA_MFT_INLINE       = 1,
	TESSERA_MFT_CHUNK_LIST   = 2,
	TESSERA_MFT_CHUNK_TREE   = 3,
	TESSERA_MFT_DIRECTORY    = 4,
	TESSERA_MFT_SYMLINK      = 5,
	TESSERA_MFT_XATTR_STORE  = 6,
	TESSERA_MFT_GC_ROOT_LIST = 7,
} tessera_manifest_kind_t;

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

/* ── B+tree node header (32 bytes) ───────────────────────────────── */

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

#define TESSERA_REGISTRY_FLAG_SEALED    (1u << 0)
#define TESSERA_REGISTRY_FLAG_RETIRING  (1u << 1)

/* ── Free-extent record (16 bytes) ───────────────────────────────── */

typedef struct TESSERA_PACKED {
	uint64_t  start_sector;
	uint64_t  length_sectors;
} tessera_free_extent_t;

/* ── Static size assertions ──────────────────────────────────────── */

#define TESSERA_STATIC_ASSERT(expr, name) \
	typedef char tessera_static_assert_##name[(expr) ? 1 : -1]

TESSERA_STATIC_ASSERT(sizeof(tessera_superblock_t)        == 4096, superblock_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_journal_header_t)    == 4096, journal_header_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_record_header_t)     == 32,   record_header_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_pack_header_t)       == 4096, pack_header_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_pack_index_entry_t)  == 48,   pack_index_entry_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_blob_descriptor_t)   == 16,   blob_descriptor_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_pack_footer_t)       == 4096, pack_footer_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_manifest_header_t)   == 32,   manifest_header_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_chunk_record_t)      == 48,   chunk_record_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_tree_record_t)       == 40,   tree_record_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_inode_record_t)      == 144,  inode_record_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_btree_node_header_t) == 32,   btree_node_header_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_registry_entry_t)    == 64,   registry_entry_size);
TESSERA_STATIC_ASSERT(sizeof(tessera_free_extent_t)       == 16,   free_extent_size);

#ifdef __cplusplus
}
#endif

#endif /* TESSERA_FORMAT_H_ */
