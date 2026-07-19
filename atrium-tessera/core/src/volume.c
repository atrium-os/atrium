/*
 * tessera-core: volume layer (mkfs + open).
 *
 * Format flow:
 *   - The caller's block_io provides read_block / write_block. alloc /
 *     free are bypassed at format time: we wrap the caller's io with a
 *     shim whose alloc bumps sectors out of a fixed metadata zone
 *     (16 sectors immediately after the journal). The bump-allocator's
 *     "free" is a no-op — sectors freed during a COW write inside the
 *     zone are simply wasted. Headroom is generous (4 vs 16) so this
 *     is safe.
 *   - Once the empty inode + pack-registry + free-extent trees are in
 *     place, the superblock is sealed (twice — sectors 0 and 1) with
 *     generation = 1 and a populated CRC.
 *
 * Open flow:
 *   - Read both superblocks. Pick the one with valid magic + CRC and
 *     the highest generation. ECORRUPT if neither is valid.
 *   - Validate version_major == 1 and sector_size == 4096; load roots
 *     into the handle. Journal replay is a separate, Phase-3 step.
 */

#include "tessera/volume.h"
#include "tessera/btree.h"
#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera/extent.h"
#include "tessera/format.h"
#include "tessera/hash.h"
#include "tessera/journal.h"
#include "tessera/manifest.h"
#include "tessera/pack.h"

#include "tessera_compat.h"

#define DEFAULT_JOURNAL_SECTORS 256u

/* ── opaque handle ───────────────────────────────────────────────── */

struct tessera_volume {
	tessera_block_io_t    io;
	tessera_superblock_t  sb;
};

/* ── format-time bump allocator shim ─────────────────────────────── */

struct fmt_ctx {
	const tessera_block_io_t *real;
	uint64_t bump;
	uint64_t bump_max;       /* exclusive */
};

static int
fmt_read(void *ctx, uint64_t s, uint8_t *out)
{
	struct fmt_ctx *fc = ctx;
	return fc->real->read_block(fc->real->ctx, s, out);
}

static int
fmt_write(void *ctx, uint64_t s, const uint8_t *b)
{
	struct fmt_ctx *fc = ctx;
	return fc->real->write_block(fc->real->ctx, s, b);
}

static int
fmt_alloc(void *ctx, uint64_t n, uint64_t *out)
{
	struct fmt_ctx *fc = ctx;
	if (fc->bump + n > fc->bump_max) return -1;
	*out = fc->bump;
	fc->bump += n;
	return 0;
}

static int
fmt_free(void *ctx, uint64_t s, uint64_t n)
{
	(void)ctx; (void)s; (void)n;
	return 0;
}

/* ── format-time root-directory seeder ───────────────────────────── */

/* Encode a u32 inode_no as a big-endian B+tree key. Same convention
 * the kmod and the property tests use. */
static void
encode_inode_key_be(uint32_t inode_no, uint8_t out[4])
{
	out[0] = (uint8_t)(inode_no >> 24);
	out[1] = (uint8_t)(inode_no >> 16);
	out[2] = (uint8_t)(inode_no >>  8);
	out[3] = (uint8_t)(inode_no      );
}

/* Pre-populate the volume with a single dirent in the root directory.
 *
 * Builds a 1-entry DIRECTORY manifest, packages it into a tiny pack
 * file, allocates pack-zone sectors via the live extent allocator,
 * writes the pack via the bump shim, registers it, inserts the child
 * inode as a regular file, and re-puts inode 2 with manifest_hash
 * pointing at the new directory blob. All four roots
 * (inode_root, pack_root, free root via the extent flush done by the
 * caller) are advanced in place via the *_root out-pointers. */
static int
seed_root_dirent(const tessera_block_io_t *shim,
                 tessera_extent_alloc_t *ea,
                 tessera_btree_t *inode_tree,
                 tessera_btree_t *pack_tree,
                 uint64_t *inode_root,
                 uint64_t *pack_root,
                 const tessera_format_opts_t *opts)
{
	const int has_seed = (opts->seed_dirent_name != NULL &&
	                      opts->seed_dirent_name_len > 0);
	if (has_seed && opts->seed_dirent_inode < TESSERA_INODE_FIRST_USER)
		return TESSERA_EINVAL;

	int r;

	/* 1. Build the DIRECTORY manifest. Always run — even with no
	 * seed file, we need an empty DIRECTORY blob so inode 2's
	 * manifest_hash points at a valid (zero-entry) directory.
	 * Without this, the kmod's vop_create / vop_lookup hit the
	 * `manifest_hash all zero` branch and return ENOENT for any
	 * name lookup, breaking the canonical "fresh mkfs + touch
	 * file" workflow. */
	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
	if (mb != NULL)
		(void)tessera_manifest_set_hash_alg(mb, opts->hash_alg);
	if (mb == NULL) return TESSERA_ENOMEM;
	if (has_seed) {
		r = tessera_manifest_add_dirent(mb, opts->seed_dirent_inode,
		    opts->seed_dirent_name, opts->seed_dirent_name_len);
		if (r != TESSERA_OK) { tessera_manifest_free(mb); return r; }
	}

	size_t mft_size = 0;
	tessera_hash_t mft_hash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mft_size, mft_hash);
	uint8_t *mft_bytes = tessera_malloc(mft_size);
	if (mft_bytes == NULL) {
		tessera_manifest_free(mb);
		return TESSERA_ENOMEM;
	}
	r = tessera_manifest_finalize(mb, mft_bytes, mft_size,
	    &mft_size, mft_hash);
	tessera_manifest_free(mb);
	if (r != TESSERA_OK) { tessera_free(mft_bytes); return r; }

	/* 1b. Optional file-content manifest. INLINE if chunk_size == 0,
	 * CHUNK_LIST otherwise. For CHUNK_LIST the chunk bytes are added
	 * to the pack as separate CHUNK blobs further down. */
	const int has_content = (opts->seed_content_data != NULL &&
	                         opts->seed_content_len  > 0);
	const int chunked = has_content && opts->seed_chunk_size > 0;
	const size_t n_chunks = chunked
	    ? (opts->seed_content_len + opts->seed_chunk_size - 1u)
	      / opts->seed_chunk_size
	    : 0;
	uint8_t *inl_bytes = NULL;
	size_t   inl_size  = 0;
	tessera_hash_t inl_hash;
	tessera_hash_t *chunk_hashes = NULL;
	memset(inl_hash, 0, sizeof inl_hash);
	if (has_content) {
		tessera_manifest_kind_t kind = chunked
		    ? TESSERA_MFT_CHUNK_LIST
		    : TESSERA_MFT_INLINE;
		tessera_manifest_builder_t *ib = tessera_manifest_begin(kind);
	if (ib != NULL) {
		(void)tessera_manifest_set_hash_alg(ib, opts->hash_alg);
	}
		if (ib == NULL) {
			tessera_free(mft_bytes);
			return TESSERA_ENOMEM;
		}
		if (chunked) {
			chunk_hashes = tessera_calloc(n_chunks, sizeof *chunk_hashes);
			if (chunk_hashes == NULL) {
				tessera_manifest_free(ib);
				tessera_free(mft_bytes);
				return TESSERA_ENOMEM;
			}
			size_t off = 0;
			for (size_t i = 0; i < n_chunks; i++) {
				size_t cs = (off + opts->seed_chunk_size >
				             opts->seed_content_len)
				    ? opts->seed_content_len - off
				    : opts->seed_chunk_size;
				tessera_content_hash(opts->hash_alg,
				    opts->seed_content_data + off,
				    cs, chunk_hashes[i]);
				r = tessera_manifest_add_chunk(ib,
				    chunk_hashes[i], off, (uint32_t)cs, 0);
				if (r != TESSERA_OK) {
					tessera_manifest_free(ib);
					tessera_free(chunk_hashes);
					tessera_free(mft_bytes);
					return r;
				}
				off += cs;
			}
		} else {
			r = tessera_manifest_set_inline(ib,
			    opts->seed_content_data, opts->seed_content_len);
			if (r != TESSERA_OK) {
				tessera_manifest_free(ib);
				tessera_free(mft_bytes);
				return r;
			}
		}
		(void)tessera_manifest_finalize(ib, NULL, 0, &inl_size, inl_hash);
		inl_bytes = tessera_malloc(inl_size);
		if (inl_bytes == NULL) {
			tessera_manifest_free(ib);
			tessera_free(chunk_hashes);
			tessera_free(mft_bytes);
			return TESSERA_ENOMEM;
		}
		r = tessera_manifest_finalize(ib, inl_bytes, inl_size,
		    &inl_size, inl_hash);
		tessera_manifest_free(ib);
		if (r != TESSERA_OK) {
			tessera_free(inl_bytes);
			tessera_free(chunk_hashes);
			tessera_free(mft_bytes);
			return r;
		}
	}

	/* 2. Build a pack containing the directory manifest (and the
	 *    INLINE file-content manifest if seeded). */
	uint8_t pack_id[16] = { 'T','S','D','0', /* recognisable in dumps */
	                        0,0,0,0, 0,0,0,0, 0,0,0,1 };
	tessera_pack_builder_t *pb =
	    tessera_pack_begin(/*kind*/ 0, pack_id, /*creator_tx*/ 0);
	if (pb == NULL) {
		tessera_free(inl_bytes); tessera_free(chunk_hashes); tessera_free(mft_bytes);
		return TESSERA_ENOMEM;
	}
	r = tessera_pack_add_blob(pb, mft_hash, mft_bytes,
	    (uint32_t)mft_size, TESSERA_BLOB_FLAG_MANIFEST);
	if (r != TESSERA_OK) {
		tessera_pack_free(pb);
		tessera_free(inl_bytes); tessera_free(chunk_hashes); tessera_free(mft_bytes);
		return r;
	}
	if (has_content) {
		r = tessera_pack_add_blob(pb, inl_hash, inl_bytes,
		    (uint32_t)inl_size, TESSERA_BLOB_FLAG_MANIFEST);
		if (r != TESSERA_OK) {
			tessera_pack_free(pb);
			tessera_free(inl_bytes); tessera_free(chunk_hashes);
			tessera_free(mft_bytes);
			return r;
		}
	}
	if (chunked) {
		size_t off = 0;
		for (size_t i = 0; i < n_chunks; i++) {
			size_t cs = (off + opts->seed_chunk_size >
			             opts->seed_content_len)
			    ? opts->seed_content_len - off
			    : opts->seed_chunk_size;
			r = tessera_pack_add_blob(pb, chunk_hashes[i],
			    opts->seed_content_data + off, (uint32_t)cs,
			    TESSERA_BLOB_FLAG_CHUNK);
			if (r != TESSERA_OK) {
				/* Test images use simple unique-chunk content;
				 * production callers de-dup before adding. */
				tessera_pack_free(pb);
				tessera_free(inl_bytes);
				tessera_free(chunk_hashes);
				tessera_free(mft_bytes);
				return r;
			}
			off += cs;
		}
	}
	size_t pack_size = 0;
	(void)tessera_pack_finalize(pb, NULL, 0, &pack_size);
	uint8_t *pack_bytes = tessera_malloc(pack_size);
	if (pack_bytes == NULL) {
		tessera_pack_free(pb);
		tessera_free(inl_bytes); tessera_free(chunk_hashes);
		tessera_free(mft_bytes);
		return TESSERA_ENOMEM;
	}
	r = tessera_pack_finalize(pb, pack_bytes, pack_size, &pack_size);
	tessera_pack_free(pb);
	if (r != TESSERA_OK) {
		tessera_free(pack_bytes);
		tessera_free(inl_bytes); tessera_free(chunk_hashes); tessera_free(mft_bytes);
		return r;
	}
	if ((pack_size % TESSERA_SECTOR_SIZE) != 0) {
		tessera_free(pack_bytes);
		tessera_free(inl_bytes); tessera_free(chunk_hashes); tessera_free(mft_bytes);
		return TESSERA_ECORRUPT;
	}

	/* 3. Allocate pack-zone sectors. */
	const uint64_t n_sectors = pack_size / TESSERA_SECTOR_SIZE;
	uint64_t pack_start = 0;
	r = tessera_extent_alloc(ea, n_sectors, &pack_start);
	if (r != TESSERA_OK) {
		tessera_free(pack_bytes);
		tessera_free(inl_bytes); tessera_free(chunk_hashes); tessera_free(mft_bytes);
		return r;
	}

	/* 4. Write pack bytes through the shim. */
	for (uint64_t i = 0; i < n_sectors; i++) {
		if (shim->write_block(shim->ctx, pack_start + i,
		    pack_bytes + i * TESSERA_SECTOR_SIZE) != 0) {
			tessera_free(pack_bytes);
			tessera_free(inl_bytes); tessera_free(chunk_hashes); tessera_free(mft_bytes);
			return TESSERA_EIO;
		}
	}
	tessera_free(pack_bytes);

	/* 5. Insert pack registry entry. */
	{
		tessera_registry_entry_t re;
		memset(&re, 0, sizeof re);
		memcpy(re.pack_id, pack_id, 16);
		re.start_sector    = pack_start;
		re.length_sectors  = n_sectors;
		re.blob_count      = (uint32_t)(1 + (has_content ? 1u : 0u)
		                                + n_chunks);
		re.pack_kind       = 0;
		re.total_bytes     = pack_size;
		re.create_time     = 0;
		re.reachable_blobs = re.blob_count;
		re.flags           = TESSERA_REGISTRY_FLAG_SEALED;
		uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
		r = tessera_encode_registry_entry(&re, reg_value);
		if (r != TESSERA_OK) {
			tessera_free(inl_bytes); tessera_free(chunk_hashes); tessera_free(mft_bytes);
			return r;
		}
		r = tessera_btree_put(pack_tree, pack_id, reg_value, pack_root);
		if (r != TESSERA_OK) {
			tessera_free(inl_bytes); tessera_free(chunk_hashes); tessera_free(mft_bytes);
			return r;
		}
	}

	/* 6. Insert child inode (regular file) — only if a seed dirent
	 *    was requested. The empty-DIRECTORY-manifest path runs above
	 *    unconditionally so inode 2 always has a valid manifest_hash. */
	if (has_seed) {
		tessera_inode_record_t ino;
		memset(&ino, 0, sizeof ino);
		ino.inode_no = (uint32_t)opts->seed_dirent_inode;
		ino.gen      = 1;
		ino.mode     = 0100644;       /* S_IFREG | 0644 */
		ino.nlink    = 1;
		if (has_content) {
			ino.size = opts->seed_content_len;
			memcpy(ino.manifest_hash, inl_hash, 32);
		}
		uint8_t key[4];
		encode_inode_key_be((uint32_t)opts->seed_dirent_inode, key);
		uint8_t value[TESSERA_INODE_RECORD_SIZE];
		r = tessera_encode_inode(&ino, value);
		if (r != TESSERA_OK) {
			tessera_free(inl_bytes); tessera_free(chunk_hashes); tessera_free(mft_bytes);
			return r;
		}
		r = tessera_btree_put(inode_tree, key, value, inode_root);
		if (r != TESSERA_OK) {
			tessera_free(inl_bytes); tessera_free(chunk_hashes); tessera_free(mft_bytes);
			return r;
		}
	}

	/* 7. Re-put inode 2 with manifest_hash now pointing at the new dir. */
	{
		tessera_inode_record_t ino;
		memset(&ino, 0, sizeof ino);
		ino.inode_no = TESSERA_INODE_ROOT_DIR;
		ino.gen      = 2;
		ino.mode     = 040755;
		ino.nlink    = 2;
		memcpy(ino.manifest_hash, mft_hash, 32);
		uint8_t key[4];
		encode_inode_key_be(TESSERA_INODE_ROOT_DIR, key);
		uint8_t value[TESSERA_INODE_RECORD_SIZE];
		r = tessera_encode_inode(&ino, value);
		if (r != TESSERA_OK) { tessera_free(mft_bytes); return r; }
		r = tessera_btree_put(inode_tree, key, value, inode_root);
		if (r != TESSERA_OK) { tessera_free(mft_bytes); return r; }
	}

	tessera_free(inl_bytes);
	tessera_free(mft_bytes);
	return TESSERA_OK;
}

/* ── format ──────────────────────────────────────────────────────── */

int
tessera_volume_format(const tessera_block_io_t *io,
                      const tessera_format_opts_t *opts)
{
	if (io == NULL || opts == NULL) return TESSERA_EINVAL;
	if (opts->hash_alg != TESSERA_HASH_ALG_SHA256 &&
	    opts->hash_alg != TESSERA_HASH_ALG_BLAKE3_256)
		return TESSERA_EINVAL;
	if (io->read_block == NULL || io->write_block == NULL)
		return TESSERA_EINVAL;

	const uint64_t J = opts->journal_sectors ? opts->journal_sectors
	                                          : DEFAULT_JOURNAL_SECTORS;
	const uint64_t metadata_zone_start = 4 + J;
	/* Scale meta-reserve with volume size (max of the historical
	 * 1024-sector floor and 1.5% of total). 4 MiB was tight under
	 * stress2's 4-incarnation parallel mkdir/creat — pending
	 * sectors filled up faster than commit_sb could drain them. */
	uint64_t meta_zone_sectors = TESSERA_METADATA_ZONE_SECTORS;
	uint64_t meta_scaled = opts->total_sectors / 16;  /* ~6% */
	if (meta_scaled > meta_zone_sectors) meta_zone_sectors = meta_scaled;
	const uint64_t free_zone_start =
	    metadata_zone_start + meta_zone_sectors;

	if (J < 4) return TESSERA_EINVAL;
	if (opts->total_sectors <= free_zone_start + 1)
		return TESSERA_EINVAL;

	/* 1. Format the journal. */
	int r = tessera_journal_format(io, 4, J);
	if (r != TESSERA_OK) return r;

	/* 2. Set up the bump-allocator shim. */
	struct fmt_ctx fc = {
		.real     = io,
		.bump     = metadata_zone_start,
		.bump_max = free_zone_start,
	};
	tessera_block_io_t shim = {
		.read_block  = fmt_read,
		.write_block = fmt_write,
		.alloc       = fmt_alloc,
		.free        = fmt_free,
		.ctx         = &fc,
	};

	/* 3. Create empty inode B+tree (key = u32 inode_no big-endian so
	 * memcmp ordering matches numeric ordering, value = 144-byte
	 * inode record). */
	uint64_t inode_root = 0;
	tessera_btree_t *t1 = tessera_btree_create(&shim, /*kind*/ 0,
	    /*key*/ 4, /*value*/ TESSERA_INODE_RECORD_SIZE, &inode_root);
	if (t1 == NULL) return TESSERA_ENOSPC;

	/* 3a. Seed inode 2 (the root directory). manifest_hash + xattr_hash
	 * are left all-zero — sentinel for "empty directory" until step
	 * 5a (below) publishes a real DIRECTORY manifest blob. */
	{
		tessera_inode_record_t ino;
		memset(&ino, 0, sizeof ino);
		ino.inode_no = TESSERA_INODE_ROOT_DIR;
		ino.gen      = 1;
		ino.mode     = 040755;        /* S_IFDIR | 0755 */
		ino.uid      = 0;
		ino.gid      = 0;
		ino.nlink    = 2;
		ino.size     = 0;

		uint8_t key[4];
		encode_inode_key_be(TESSERA_INODE_ROOT_DIR, key);
		uint8_t value[TESSERA_INODE_RECORD_SIZE];
		r = tessera_encode_inode(&ino, value);
		if (r != TESSERA_OK) {
			tessera_btree_close(t1);
			return r;
		}
		r = tessera_btree_put(t1, key, value, &inode_root);
		if (r != TESSERA_OK) {
			tessera_btree_close(t1);
			return r;
		}
	}
	/* t1 stays open through the seed step. */

	/* 4. Create empty pack-registry B+tree (key = 16-byte pack_id,
	 * value = 64-byte registry entry). */
	uint64_t pack_registry_root = 0;
	tessera_btree_t *t2 = tessera_btree_create(&shim, /*kind*/ 1,
	    /*key*/ 16, /*value*/ TESSERA_REGISTRY_ENTRY_SIZE,
	    &pack_registry_root);
	if (t2 == NULL) {
		tessera_btree_close(t1);
		return TESSERA_ENOSPC;
	}

	/* 5. Seed the free-extent allocator with the post-metadata free
	 * zone (don't flush yet — the seed step below allocates from it). */
	tessera_extent_alloc_t *ea = tessera_extent_open(&shim, 0);
	if (ea == NULL) {
		tessera_btree_close(t2); tessera_btree_close(t1);
		return TESSERA_ENOMEM;
	}
	r = tessera_extent_free(ea, free_zone_start,
	    opts->total_sectors - free_zone_start);
	if (r != TESSERA_OK) {
		tessera_extent_close(ea);
		tessera_btree_close(t2); tessera_btree_close(t1);
		return r;
	}

	/* 5a. Publish the root directory's manifest. Always runs — even
	 * with no seed file, we emit an empty DIRECTORY manifest pack
	 * and re-put inode 2 with that hash. Without this, freshly-
	 * formatted volumes left inode 2.manifest_hash all-zero, which
	 * the kmod's vop_lookup / vop_create paths treat as "no such
	 * directory" — so `touch /mnt/tessera/foo` immediately after
	 * mkfs returned ENOENT. The optional seed dirent + child inode
	 * are added inside seed_root_dirent when seed_dirent_name is
	 * non-NULL. */
	r = seed_root_dirent(&shim, ea, t1, t2,
	    &inode_root, &pack_registry_root, opts);
	if (r != TESSERA_OK) {
		tessera_extent_close(ea);
		tessera_btree_close(t2); tessera_btree_close(t1);
		return r;
	}

	tessera_btree_close(t2);
	tessera_btree_close(t1);

	/* 6. Flush the free-extent allocator (now reflects any pack-zone
	 * sectors consumed by 5a) as the on-disk free-extent tree. */
	uint64_t free_extent_root = 0;
	r = tessera_extent_flush(ea, &free_extent_root);
	tessera_extent_close(ea);
	if (r != TESSERA_OK) return r;

	/* 6. Seal the superblock; write twice (A and B). */
	tessera_superblock_t sb;
	memset(&sb, 0, sizeof sb);
	memcpy(sb.magic, TESSERA_MAGIC_SUPERBLOCK, 8);
	sb.version_major          = 1;
	sb.version_minor          = 0;
	sb.feature_flags          = 0;
	sb.incompat_flags         = (opts->hash_alg != TESSERA_HASH_ALG_SHA256)
	    ? TESSERA_INCOMPAT_HASH_ALG : 0;
	sb.hash_alg               = opts->hash_alg;
	sb.generation             = 1;
	memcpy(sb.volume_uuid, opts->volume_uuid, 16);
	sb.total_sectors          = opts->total_sectors;
	sb.sector_size            = TESSERA_SECTOR_SIZE;
	sb.journal_start          = 4;
	sb.journal_length         = J;
	sb.inode_root             = inode_root;
	sb.inode_root_generation  = 1;
	sb.pack_registry_root     = pack_registry_root;
	sb.pack_registry_gen      = 1;
	sb.free_extent_root       = free_extent_root;
	sb.free_extent_gen        = 1;
	sb.pack_zone_start        = free_zone_start;
	sb.pack_zone_length       = opts->total_sectors - free_zone_start;
	sb.meta_reserve_start     = metadata_zone_start;
	sb.meta_reserve_length    = meta_zone_sectors;
	sb.meta_reserve_bump      = fc.bump;       /* runtime starts here */
	sb.next_inode_no          =
	    (opts->seed_dirent_name != NULL && opts->seed_dirent_name_len > 0)
	        ? (uint64_t)opts->seed_dirent_inode + 1
	        : TESSERA_INODE_FIRST_USER;
	sb.last_unmount_clean     = 1;

	uint8_t buf[TESSERA_SECTOR_SIZE];
	r = tessera_encode_superblock(&sb, buf);
	if (r != TESSERA_OK) return r;
	if (io->write_block(io->ctx, 0, buf) != 0) return TESSERA_EIO;
	if (io->write_block(io->ctx, 1, buf) != 0) return TESSERA_EIO;

	return TESSERA_OK;
}

/* ── offline commit (fsck-repair) ─────────────────────────────────── */

/*
 * Seal a fresh superblock from the volume's active SB with a repaired
 * set of roots and write both SB copies. This is the offline analogue
 * of the kmod's commit_sb tail — no journal, no barriers beyond the
 * fd's own durability (tessera-fsck opens the device O_SYNC, so every
 * btree / pack / free-tree sector the repair wrote is already durable
 * before we get here). Bumping `generation` makes tessera_volume_open
 * prefer the repaired SB; writing BOTH slots keeps A and B in lockstep
 * so a crash mid-commit still opens a self-consistent volume (either
 * the old gen from an un-updated slot, or the new gen whose roots point
 * at already-durable structures).
 *
 * Only the fields a repair can legitimately change are overridden; all
 * other SB state (uuid, zones, journal, hash_alg, encryption, …) is
 * preserved verbatim. A per-root generation counter is bumped only for
 * the roots that actually moved, so `tessera-stat` still reflects which
 * structure the repair rewrote.
 */
int
tessera_volume_commit_roots(tessera_volume_t *v,
                            const tessera_commit_roots_t *roots)
{
	if (v == NULL || roots == NULL) return TESSERA_EINVAL;
	if (v->io.write_block == NULL) return TESSERA_EINVAL;

	tessera_superblock_t sb = v->sb;   /* preserve everything */

	sb.generation += 1;
	if (roots->inode_root != sb.inode_root) {
		sb.inode_root = roots->inode_root;
		sb.inode_root_generation += 1;
	}
	if (roots->pack_registry_root != sb.pack_registry_root) {
		sb.pack_registry_root = roots->pack_registry_root;
		sb.pack_registry_gen += 1;
	}
	if (roots->free_extent_root != sb.free_extent_root) {
		sb.free_extent_root = roots->free_extent_root;
		sb.free_extent_gen += 1;
	}
	if (roots->quota_tree_root != sb.quota_tree_root) {
		sb.quota_tree_root = roots->quota_tree_root;
		sb.quota_tree_gen += 1;
	}
	if (roots->snapshots_root != sb.snapshots_root) {
		sb.snapshots_root = roots->snapshots_root;
		sb.snapshots_gen += 1;
	}
	/* meta_reserve_bump only ever grows (repair consumed reserve
	 * sectors for the rewritten trees); never let it regress. */
	if (roots->meta_reserve_bump > sb.meta_reserve_bump)
		sb.meta_reserve_bump = roots->meta_reserve_bump;
	/* next_inode_no may grow if repair minted an inode (lost+found). */
	if (roots->next_inode_no > sb.next_inode_no)
		sb.next_inode_no = roots->next_inode_no;
	sb.last_unmount_clean = 1;

	uint8_t buf[TESSERA_SECTOR_SIZE];
	int r = tessera_encode_superblock(&sb, buf);
	if (r != TESSERA_OK) return r;
	if (v->io.write_block(v->io.ctx, 0, buf) != 0) return TESSERA_EIO;
	if (v->io.write_block(v->io.ctx, 1, buf) != 0) return TESSERA_EIO;

	v->sb = sb;   /* keep the handle coherent for any further work */
	return TESSERA_OK;
}

/* ── open ────────────────────────────────────────────────────────── */

int
tessera_volume_open(const tessera_block_io_t *io, tessera_volume_t **out)
{
	if (io == NULL || out == NULL) return TESSERA_EINVAL;
	if (io->read_block == NULL) return TESSERA_EINVAL;

	uint8_t  buf_a[TESSERA_SECTOR_SIZE], buf_b[TESSERA_SECTOR_SIZE];
	tessera_superblock_t sb_a, sb_b;
	int valid_a = (io->read_block(io->ctx, 0, buf_a) == 0) &&
	    tessera_decode_superblock(buf_a, &sb_a) == TESSERA_OK;
	int valid_b = (io->read_block(io->ctx, 1, buf_b) == 0) &&
	    tessera_decode_superblock(buf_b, &sb_b) == TESSERA_OK;

	if (!valid_a && !valid_b) return TESSERA_ECORRUPT;
	const tessera_superblock_t *active;
	if (valid_a && valid_b)
		active = (sb_a.generation >= sb_b.generation) ? &sb_a : &sb_b;
	else
		active = valid_a ? &sb_a : &sb_b;

	if (active->version_major != 1)            return TESSERA_EBADVERSION;
	if (active->sector_size != TESSERA_SECTOR_SIZE) return TESSERA_EBADVERSION;
	if ((active->incompat_flags & ~TESSERA_INCOMPAT_HASH_ALG) != 0)
		return TESSERA_EINCOMPAT;
	if (active->hash_alg != TESSERA_HASH_ALG_SHA256 &&
	    active->hash_alg != TESSERA_HASH_ALG_BLAKE3_256)
		return TESSERA_EINCOMPAT;
	/* Non-default alg must carry the incompat bit (and vice versa) —
	 * a mismatch means a corrupt or hand-edited superblock. */
	if ((active->hash_alg != TESSERA_HASH_ALG_SHA256) !=
	    ((active->incompat_flags & TESSERA_INCOMPAT_HASH_ALG) != 0))
		return TESSERA_ECORRUPT;

	tessera_volume_t *v = tessera_zalloc(sizeof *v);
	if (v == NULL) return TESSERA_ENOMEM;
	v->io = *io;
	v->sb = *active;
	*out = v;
	return TESSERA_OK;
}

void
tessera_volume_close(tessera_volume_t *v)
{
	tessera_free(v);
}

/* ── accessors ───────────────────────────────────────────────────── */

uint64_t tessera_volume_total_sectors(const tessera_volume_t *v)
{ return v ? v->sb.total_sectors : 0; }

uint64_t tessera_volume_generation(const tessera_volume_t *v)
{ return v ? v->sb.generation : 0; }

uint64_t tessera_volume_inode_root(const tessera_volume_t *v)
{ return v ? v->sb.inode_root : 0; }

uint64_t tessera_volume_pack_registry_root(const tessera_volume_t *v)
{ return v ? v->sb.pack_registry_root : 0; }

uint64_t tessera_volume_free_extent_root(const tessera_volume_t *v)
{ return v ? v->sb.free_extent_root : 0; }

uint64_t tessera_volume_journal_start(const tessera_volume_t *v)
{ return v ? v->sb.journal_start : 0; }

uint64_t tessera_volume_journal_length(const tessera_volume_t *v)
{ return v ? v->sb.journal_length : 0; }

const uint8_t *tessera_volume_uuid(const tessera_volume_t *v)
{ return v ? v->sb.volume_uuid : NULL; }

uint32_t tessera_volume_hash_alg(const tessera_volume_t *v)
{ return v ? v->sb.hash_alg : 0; }
uint64_t tessera_volume_snapshots_root(const tessera_volume_t *v)
{ return v ? v->sb.snapshots_root : 0; }

uint64_t tessera_volume_snapshots_gen(const tessera_volume_t *v)
{ return v ? v->sb.snapshots_gen : 0; }

uint64_t tessera_volume_meta_reserve_start(const tessera_volume_t *v)
{ return v ? v->sb.meta_reserve_start : 0; }

uint64_t tessera_volume_meta_reserve_length(const tessera_volume_t *v)
{ return v ? v->sb.meta_reserve_length : 0; }

uint64_t tessera_volume_meta_reserve_bump(const tessera_volume_t *v)
{ return v ? v->sb.meta_reserve_bump : 0; }

uint64_t tessera_volume_pack_zone_start(const tessera_volume_t *v)
{ return v ? v->sb.pack_zone_start : 0; }

uint64_t tessera_volume_pack_zone_length(const tessera_volume_t *v)
{ return v ? v->sb.pack_zone_length : 0; }

uint16_t tessera_volume_encryption_flags(const tessera_volume_t *v)
{ return v ? v->sb.encryption_flags : 0; }

uint8_t tessera_volume_active_slot_count(const tessera_volume_t *v)
{ return v ? v->sb.active_slot_count : 0; }

uint64_t tessera_volume_quota_tree_root(const tessera_volume_t *v)
{ return v ? v->sb.quota_tree_root : 0; }

uint64_t tessera_volume_next_inode_no(const tessera_volume_t *v)
{ return v ? v->sb.next_inode_no : 0; }
