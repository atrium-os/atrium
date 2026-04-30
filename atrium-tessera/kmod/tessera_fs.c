/*
 * tessera_fs: FreeBSD VFS module for the Tessera content-addressed
 * filesystem.
 *
 * Phase 4 round 2: real backing-block-device mount.
 *
 *   Mount accepts a "from" path naming a block device (typically a
 *   /dev/mdN created by mdconfig(8) wrapping a Tessera image file
 *   produced by mkfs-tessera(1)). We resolve it via namei, verify it
 *   IS a block device, attach via g_vfs_open, then read sector 0
 *   (and 1 as fallback) via bread() and decode the superblock with
 *   tessera_decode_superblock. Subsequent rounds use the same
 *   bread() path for tree walks.
 *
 *   Synthesized empty root directory is unchanged from round 1; the
 *   tree-walk code that fills it in lands once the inode B+tree path
 *   is wired up.
 *
 *   Spec: docs/spec/tessera-fs.md (on-disk format)
 *         docs/spec/tessera-vfs.md (POSIX mapping)
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>
#include <sys/module.h>
#include <sys/mount.h>
#include <sys/vnode.h>
#include <sys/dirent.h>
#include <sys/stat.h>
#include <sys/lockmgr.h>
#include <sys/namei.h>
#include <sys/proc.h>
#include <sys/sysctl.h>
#include <sys/malloc.h>
#include <sys/uio.h>
#include <sys/types.h>
#include <sys/buf.h>
#include <sys/conf.h>
#include <sys/priv.h>
#include <sys/fcntl.h>
#include <sys/callout.h>
#include <sys/taskqueue.h>
#include <geom/geom.h>
#include <geom/geom_vfs.h>

#include "tessera/btree.h"
#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera/extent.h"
#include "tessera/format.h"
#include "tessera/hash.h"
#include "tessera/journal.h"
#include "tessera/manifest.h"
#include "tessera/pack.h"
#include "tessera/volume.h"

MALLOC_DEFINE(M_TESSERA, "tessera", "Tessera filesystem");

/*
 * Debug knobs for testing crash-recovery paths.
 *
 *   kern.tessera.skip_next_sb = 1
 *     Cause the very next tessera_commit_sb to journal+checkpoint
 *     normally but SKIP the SB sector writes. Auto-clears to 0 after
 *     it fires. This simulates a power-loss between tx_commit and
 *     the SB-write barrier — exactly the gap that
 *     replay-on-mount is supposed to cover.
 *
 * Manual use: `sysctl kern.tessera.skip_next_sb=1` from userspace
 * before triggering the mutation you want to "crash" mid-commit,
 * then umount + remount and observe the replay log line.
 */
static SYSCTL_NODE(_kern, OID_AUTO, tessera,
    CTLFLAG_RW | CTLFLAG_MPSAFE, NULL, "Tessera filesystem");
static int tessera_skip_next_sb = 0;
SYSCTL_INT(_kern_tessera, OID_AUTO, skip_next_sb,
    CTLFLAG_RW, &tessera_skip_next_sb, 0,
    "Skip the next commit_sb's SB sector writes (crash injection)");

/*
 * v2-step-2a: deferred SB commit (write-back of the SB sector pair).
 *
 * v1 made every successful mutation durable before returning by calling
 * tessera_commit_sb(): write a journal ROOT_UPDATE record, write SB-A
 * and SB-B (5 sector writes per syscall on the hot path). v2 batches
 * those into one commit per flush window. Per-syscall durability
 * becomes "fsync'd ops are durable" (POSIX-conformant; matches every
 * other modern FS).
 *
 * Deferred path:
 *   tessera_fs_mark_dirty()        — vops call this in place of
 *                                    commit_sb. Sets sb_dirty=1 and
 *                                    arms the per-mount flush callout.
 *   tessera_fs_flush()             — does the actual commit_sb. Called
 *                                    from vop_fsync, unmount, and the
 *                                    callout-triggered taskqueue.
 *   flush_co + flush_task          — periodic timer + deferred work
 *                                    item; commit_sb does I/O so we
 *                                    can't run it from callout(9).
 *
 * Crash before flush: old SB stays on disk; in-flight in-memory tree
 * mutations (already written to btree node sectors via kbio) become
 * orphans, reclaimed by the round-7-step-3 meta-reserve recycler and
 * round-7-step-4 data-zone GC at next mount. No corruption.
 */
static int tessera_flush_interval_sec = 5;
SYSCTL_INT(_kern_tessera, OID_AUTO, flush_interval_sec,
    CTLFLAG_RW, &tessera_flush_interval_sec, 0,
    "Seconds between deferred-SB-commit flushes");

/* Observability: per-mount counters get summed here. Use to confirm
 * batching is actually happening (sb_commits << mark_dirty calls). */
static unsigned long tessera_stat_sb_commits = 0;
static unsigned long tessera_stat_mark_dirty = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, sb_commits,
    CTLFLAG_RD, &tessera_stat_sb_commits, 0,
    "Cumulative tessera_commit_sb invocations across all mounts");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, mark_dirty,
    CTLFLAG_RD, &tessera_stat_mark_dirty, 0,
    "Cumulative tessera_fs_mark_dirty invocations");

/* v2 publish-cache observability — increments on every short-circuit
 * via existing pack_registry hit (no extent_alloc / no bwrite). */
static unsigned long tessera_stat_publish_dedup_manifest = 0;
static unsigned long tessera_stat_publish_dedup_chunked  = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, publish_dedup_manifest,
    CTLFLAG_RD, &tessera_stat_publish_dedup_manifest, 0,
    "publish_manifest calls satisfied by existing pack_registry entry");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, publish_dedup_chunked,
    CTLFLAG_RD, &tessera_stat_publish_dedup_chunked, 0,
    "publish_chunked calls satisfied by existing pack_registry entry");

/* v2 step-3 chunked-write observability — per-write-path counters
 * so future perf work can tell whether a workload is hitting the fast
 * paths or thrashing the slow rebuild path. RD-only; cumulative
 * across all mounts on this kmod load. */
static unsigned long tessera_stat_vop_write_inline    = 0;
static unsigned long tessera_stat_vop_write_chunked   = 0;
static unsigned long tessera_stat_chunk_dedup_skip    = 0;
static unsigned long tessera_stat_chunk_zero_hole     = 0;
static unsigned long tessera_stat_append_fast_ok      = 0;
static unsigned long tessera_stat_append_fast_fallback = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, vop_write_inline,
    CTLFLAG_RD, &tessera_stat_vop_write_inline, 0,
    "vop_write completions via INLINE manifest path");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, vop_write_chunked,
    CTLFLAG_RD, &tessera_stat_vop_write_chunked, 0,
    "vop_write completions via CHUNK_LIST manifest path");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, chunk_dedup_skip,
    CTLFLAG_RD, &tessera_stat_chunk_dedup_skip, 0,
    "Chunks skipped because slot's old hash already matched");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, chunk_zero_hole,
    CTLFLAG_RD, &tessera_stat_chunk_zero_hole, 0,
    "ZERO_HOLE chunks emitted (sparse-file detection)");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, append_fast_ok,
    CTLFLAG_RD, &tessera_stat_append_fast_ok, 0,
    "Append fast-path successes");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, append_fast_fallback,
    CTLFLAG_RD, &tessera_stat_append_fast_fallback, 0,
    "Append fast-path fallbacks to slow rewrite path");

/* ── kmod block_io shim ──────────────────────────────────────── */

/* tessera-core's primitives talk to "disk" via tessera_block_io_t.
 * In kernel mode we back it with bread/bwrite via the GEOM consumer.
 * For round-3 read-only use the alloc/free callbacks are stubs;
 * round-4 will wire alloc to tessera_extent_alloc against the
 * volume's free-extent tree. */

/* Forward decl — bio_ctx points back at the mount so the alloc/free
 * callbacks can route through the volume's extent allocator. */
struct tessera_mount;

struct tessera_kbio_ctx {
	struct vnode         *devvp;
	struct ucred         *cred;
	struct tessera_mount *mount;
};

static int
tessera_kbio_read(void *ctx, uint64_t sector, uint8_t *out)
{
	struct tessera_kbio_ctx *k = ctx;
	struct buf *bp = NULL;
	int err = bread(k->devvp, sector * btodb(TESSERA_SECTOR_SIZE),
	    TESSERA_SECTOR_SIZE, k->cred ? k->cred : NOCRED, &bp);
	if (err != 0) { if (bp != NULL) brelse(bp); return (-1); }
	memcpy(out, bp->b_data, TESSERA_SECTOR_SIZE);
	brelse(bp);
	return (0);
}

static int
tessera_kbio_write(void *ctx, uint64_t sector, const uint8_t *buf)
{
	struct tessera_kbio_ctx *k = ctx;
	struct buf *bp = getblk(k->devvp, sector * btodb(TESSERA_SECTOR_SIZE),
	    TESSERA_SECTOR_SIZE, 0, 0, 0);
	if (bp == NULL) return (-1);
	bzero(bp->b_data, TESSERA_SECTOR_SIZE);
	memcpy(bp->b_data, buf, TESSERA_SECTOR_SIZE);
	return (bwrite(bp));
}

/*
 * Round 6c-redux: delayed-write meta variant.
 *
 * tessera_kbio_write uses bwrite which is synchronous — it sleeps on
 * GEOM completion while the file vnode is still held EXCLUSIVE,
 * which deadlocked touch in step-2 of the vop_setattr bisection.
 *
 * bdwrite just bdirty()s the buf and returns; the syncer thread will
 * flush it later under its own locking, off the syscall path. The
 * SB-write path (tessera_commit_sb) keeps the synchronous bwrite
 * because crash safety requires the SB sectors to actually hit disk
 * before the syscall returns success.
 */
static int
tessera_kbio_write_delayed(void *ctx, uint64_t sector, const uint8_t *buf)
{
	struct tessera_kbio_ctx *k = ctx;
	struct buf *bp = getblk(k->devvp, sector * btodb(TESSERA_SECTOR_SIZE),
	    TESSERA_SECTOR_SIZE, 0, 0, 0);
	if (bp == NULL) return (-1);
	bzero(bp->b_data, TESSERA_SECTOR_SIZE);
	memcpy(bp->b_data, buf, TESSERA_SECTOR_SIZE);
	bdwrite(bp);
	return (0);
}

/* Forward decl — defined inline below so we can take the mount's
 * extent_alloc field. */
static int tessera_kbio_alloc(void *ctx, uint64_t n, uint64_t *out_sector);
static int tessera_kbio_free (void *ctx, uint64_t s, uint64_t n);

/* ── forward decls (uses tessera_mount) ──────────────────────── */
struct tessera_mount;
static int tessera_fs_fetch_blob(struct tessera_mount *tmp_,
                                  const tessera_hash_t hash,
                                  uint8_t **out_buf, uint32_t *out_len);
static int tessera_commit_sb    (struct tessera_mount *tmp_);
static int tessera_commit_extent(struct tessera_mount *tmp_);
struct tessera_replay_ctx { struct tessera_mount *tmp_; int applied; };
static int tessera_replay_handler(void *ctx,
    const tessera_record_header_t *hdr, const uint8_t *body);
struct meta_mark_ctx { uint8_t *bitmap; uint64_t lo; uint64_t hi; };
static int meta_mark_visitor(void *ctx, uint64_t sector);
static int tessera_fs_gc_data_zone(struct tessera_mount *tmp_);
static int tessera_fs_read_full_content(struct tessera_mount *tmp_,
    const tessera_inode_record_t *ino,
    uint8_t **out_buf, size_t *out_size);
static int tessera_fs_replace_content(struct tessera_mount *tmp_,
    uint32_t inode_no, const uint8_t *new_bytes, size_t new_len);
static int tessera_fs_inode_unlink(struct tessera_mount *tmp_,
    uint32_t inode_no);
static int  tessera_fs_flush     (struct tessera_mount *tmp_);
static void tessera_fs_mark_dirty(struct tessera_mount *tmp_);
static void tessera_fs_flush_task(void *ctx, int pending);

/* v2-step-3a: chunked-write helpers. */
struct tessera_chunk_in {
	tessera_hash_t  hash;
	const uint8_t  *bytes;
	uint32_t        len;
};
static int tessera_fs_publish_chunked(struct tessera_mount *tmp_,
    const struct tessera_chunk_in *chunks, uint32_t n_chunks,
    const uint8_t *manifest_bytes, size_t mlen,
    tessera_hash_t out_manifest_hash);
static int tessera_fs_replace_content_chunked(struct tessera_mount *tmp_,
    uint32_t inode_no, const uint8_t *new_bytes, size_t new_len);
static int tessera_fs_append_chunked(struct tessera_mount *tmp_,
    uint32_t inode_no, const uint8_t *append_bytes, size_t append_len,
    uint32_t cs);
static void encode_inode_key(uint32_t inode_no, uint8_t out[4]);

/* ── per-mount state ─────────────────────────────────────────── */

struct tessera_mount {
	struct vnode             *devvp;     /* the block-device vnode */
	struct g_consumer        *cp;        /* GEOM consumer for I/O */
	struct cdev              *dev;
	tessera_superblock_t      sb;
	struct tessera_kbio_ctx   bio_ctx;
	tessera_block_io_t        bio;          /* data zone */
	tessera_block_io_t        meta_bio;     /* metadata reserve */
	tessera_btree_t          *inode_tree;
	tessera_btree_t          *pack_registry_tree;
	tessera_btree_t          *snapshots_tree;     /* v2: time-machine */
	tessera_extent_alloc_t   *extent_alloc;
	tessera_journal_t        *journal;
	/*
	 * Metadata-reserve recycler (Round 7-step2). The bump pointer
	 * (`sb.meta_reserve_bump`) only goes forward; orphaned sectors
	 * from COW would otherwise leak. We track:
	 *   - meta_pending: sectors freed during the current commit
	 *     cycle. NOT reusable yet — the old SB still references the
	 *     trees that include them. A crash before commit_sb leaves
	 *     them as the only correct version.
	 *   - meta_free: sectors whose old SB has been retired. Safe to
	 *     reuse on the next allocation.
	 * commit_sb's success path drains pending → free.
	 */
	uint64_t                 *meta_free;
	uint32_t                  meta_free_count;
	uint32_t                  meta_free_cap;
	uint64_t                 *meta_pending;
	uint32_t                  meta_pending_count;
	uint32_t                  meta_pending_cap;

	/* v2-step-2a: deferred-commit state. See sysctl block above. */
	int                       sb_dirty;
	int                       flush_co_init;     /* callout initialised */
	int                       flush_unmounting;  /* don't rearm callout */
	struct callout            flush_co;
	struct task               flush_task;

	/* v2 snapshots slice 2: read-only historical mount via
	 * `tessera.gen=N`. When 1, mountfs overrode sb roots from a
	 * snapshot record; mutation paths are blocked by MNT_RDONLY,
	 * mount-time GC + meta-recycler are skipped, commit_sb won't
	 * fire (sb_dirty never gets set since mark_dirty isn't called). */
	int                       readonly_snapshot;
	uint64_t                  snapshot_gen;
};

static int
tessera_kbio_alloc(void *ctx, uint64_t n, uint64_t *out_sector)
{
	struct tessera_kbio_ctx *k = ctx;
	if (k->mount == NULL || k->mount->extent_alloc == NULL)
		return (-1);
	return (tessera_extent_alloc(k->mount->extent_alloc, n, out_sector)
	    == TESSERA_OK ? 0 : -1);
}

static int
tessera_kbio_free(void *ctx, uint64_t s, uint64_t n)
{
	struct tessera_kbio_ctx *k = ctx;
	if (k->mount == NULL || k->mount->extent_alloc == NULL)
		return (-1);
	return (tessera_extent_free(k->mount->extent_alloc, s, n)
	    == TESSERA_OK ? 0 : -1);
}

/* Metadata-reserve bump allocator. Used for inode-tree / pack-
 * registry / free-extent tree updates. NOT recursing into the data
 * extent allocator avoids the iterating-while-mutating problem
 * (tessera-fs.md §3.3). */
static int
tessera_kbio_meta_alloc(void *ctx, uint64_t n, uint64_t *out_sector)
{
	struct tessera_kbio_ctx *k = ctx;
	struct tessera_mount   *tmp_ = k->mount;
	if (tmp_ == NULL) return (-1);
	if (n != 1) return (-1);
	/* Prefer recycled sectors (released by a prior commit cycle's
	 * post-success drain) before pushing the bump pointer forward. */
	if (tmp_->meta_free_count > 0) {
		*out_sector = tmp_->meta_free[--tmp_->meta_free_count];
		return (0);
	}
	const uint64_t used = tmp_->sb.meta_reserve_bump
	    - tmp_->sb.meta_reserve_start;
	if (used + n > tmp_->sb.meta_reserve_length) {
		printf("tessera_fs: meta_reserve EXHAUSTED — used=%lu of %lu, "
		    "free_count=%u, pending=%u\n",
		    (unsigned long)used,
		    (unsigned long)tmp_->sb.meta_reserve_length,
		    tmp_->meta_free_count, tmp_->meta_pending_count);
		return (-1);
	}
	*out_sector = tmp_->sb.meta_reserve_bump;
	tmp_->sb.meta_reserve_bump += n;
	return (0);
}

static int
tessera_kbio_meta_free(void *ctx, uint64_t s, uint64_t n)
{
	struct tessera_kbio_ctx *k = ctx;
	struct tessera_mount   *tmp_ = k->mount;
	if (tmp_ == NULL || n != 1) return (-1);
	/* Defer reuse until the SB swap that retires the old trees has
	 * been durable on disk. Until then this sector is still part of
	 * the on-disk-published view; reusing it would corrupt the
	 * crash-recovery snapshot. */
	if (tmp_->meta_pending_count >= tmp_->meta_pending_cap)
		return (-1);
	tmp_->meta_pending[tmp_->meta_pending_count++] = s;
	return (0);
}

#define VFSTOTESSERA(mp) ((struct tessera_mount *)((mp)->mnt_data))

/* ── per-vnode private ──────────────────────────────────────── */

struct tessera_node {
	uint64_t inode_no;
	/* Parent inode_no, tracked at descent time so vop_lookup of ".."
	 * can return the right vnode. The root vnode loops back to
	 * itself per the standard FS convention. */
	uint64_t parent_inode_no;
};

#define VTOTNODE(vp) ((struct tessera_node *)(vp)->v_data)

extern struct vop_vector tessera_vnodeops;

/* ── superblock loader ───────────────────────────────────────── */

/* Read the superblock at sector `which` (0 for SB-A, 1 for SB-B).
 * Decodes into out_sb on success; returns 0 + valid SB, or non-zero. */
static int
tessera_load_sb(struct vnode *devvp, uint64_t which,
                tessera_superblock_t *out_sb)
{
	struct buf *bp = NULL;
	int err;

	err = bread(devvp, which * btodb(TESSERA_SECTOR_SIZE),
	    TESSERA_SECTOR_SIZE, NOCRED, &bp);
	if (err != 0) {
		if (bp != NULL) brelse(bp);
		return (err);
	}
	int rc = tessera_decode_superblock((const uint8_t *)bp->b_data, out_sb);
	brelse(bp);
	if (rc != TESSERA_OK) return (EIO);
	return (0);
}

/* Write `sb` (encoded fresh — including a recomputed CRC) to sector
 * `which`. Used at mount time to self-heal a corrupt or stale
 * superblock from the surviving good copy. Synchronous bwrite — the
 * heal is a one-sector best-effort and the volume's already mounted
 * fine off the other SB if it fails. */
static int
tessera_heal_sb(struct vnode *devvp, uint64_t which,
                const tessera_superblock_t *sb)
{
	struct buf *bp = getblk(devvp, which * btodb(TESSERA_SECTOR_SIZE),
	    TESSERA_SECTOR_SIZE, 0, 0, 0);
	if (bp == NULL) return (ENOMEM);
	bzero(bp->b_data, TESSERA_SECTOR_SIZE);
	int rc = tessera_encode_superblock(sb, (uint8_t *)bp->b_data);
	if (rc != TESSERA_OK) {
		brelse(bp);
		return (EIO);
	}
	return (bwrite(bp));
}

/* ── core mount: open device + decode SB ─────────────────────── */

static int
tessera_mountfs(struct vnode *devvp, struct mount *mp, uint64_t requested_gen)
{
	struct tessera_mount *tmp_;
	struct g_consumer    *cp = NULL;
	struct cdev          *dev = devvp->v_rdev;
	struct bufobj        *bo;
	/* Heap-allocated — sizeof(tessera_superblock_t) == 4096 (per
	 * format §3). Keeping two on the stack would burn 8 KiB right
	 * at mountfs entry, eating the kstack budget that subsequent
	 * frames (tree opens, GC, commit_sb) need. */
	tessera_superblock_t *sb_a, *sb_b, *active = NULL;
	sb_a = malloc(sizeof *sb_a, M_TESSERA, M_WAITOK | M_ZERO);
	sb_b = malloc(sizeof *sb_b, M_TESSERA, M_WAITOK | M_ZERO);
	int err;

	dev_ref(dev);
	/* Open the device read-write at the GEOM layer for live mounts so
	 * the SB self-heal + commit path can write. For historical
	 * (`tessera.gen=N`) mounts we open read-only — that lets a
	 * forensic mount coexist with the live mount on the same device. */
	const int gvfs_writers = (requested_gen != 0) ? 0 : 1;
	g_topology_lock();
	err = g_vfs_open(devvp, &cp, "tessera", gvfs_writers);
	g_topology_unlock();
	VOP_UNLOCK(devvp);
	if (err != 0) {
		free(sb_a, M_TESSERA);
		free(sb_b, M_TESSERA);
		dev_rel(dev);
		return (err);
	}

	if (cp->provider->sectorsize > TESSERA_SECTOR_SIZE ||
	    (TESSERA_SECTOR_SIZE % cp->provider->sectorsize) != 0) {
		err = EINVAL;
		goto fail_close;
	}

	bo = &devvp->v_bufobj;

	int valid_a = (tessera_load_sb(devvp, 0, sb_a) == 0);
	int valid_b = (tessera_load_sb(devvp, 1, sb_b) == 0);
	if (!valid_a && !valid_b) {
		printf("tessera_fs: neither superblock decoded; refusing to mount\n");
		err = EINVAL;
		goto fail_close;
	}
	if (valid_a && valid_b)
		active = (sb_a->generation >= sb_b->generation) ? sb_a : sb_b;
	else
		active = valid_a ? sb_a : sb_b;

	if (active->version_major != 1 ||
	    active->sector_size  != TESSERA_SECTOR_SIZE) {
		printf("tessera_fs: unsupported version/sector_size\n");
		err = EINVAL;
		goto fail_close;
	}

	/* Self-heal: if either SB failed to decode, or both decoded but
	 * generations differ, rewrite the stale/corrupt copy from the
	 * active one. The dual-SB scheme only earns its keep if we keep
	 * both copies in sync — leaving a corrupt SB-A turns the volume
	 * into a single-copy gambit. Best-effort; failures are logged
	 * but don't block the mount.
	 *
	 * Skip the heal for read-only forensic mounts — the GEOM consumer
	 * was opened with no write permission. The live mount (or a
	 * future writable mount) will fix any SB asymmetry. */
	if (requested_gen != 0) {
		/* nothing */
	} else if (!valid_a) {
		if (tessera_heal_sb(devvp, 0, active) == 0)
			printf("tessera_fs: SB-A healed from SB-B (gen=%lu)\n",
			    (unsigned long)active->generation);
		else
			printf("tessera_fs: SB-A heal failed; volume mounted "
			    "off SB-B alone\n");
	} else if (!valid_b) {
		if (tessera_heal_sb(devvp, 1, active) == 0)
			printf("tessera_fs: SB-B healed from SB-A (gen=%lu)\n",
			    (unsigned long)active->generation);
		else
			printf("tessera_fs: SB-B heal failed; volume mounted "
			    "off SB-A alone\n");
	} else if (sb_a->generation != sb_b->generation) {
		uint64_t which = (active == sb_a) ? 1 : 0;
		if (tessera_heal_sb(devvp, which, active) == 0)
			printf("tessera_fs: SB-%c re-synced to gen=%lu\n",
			    which == 0 ? 'A' : 'B',
			    (unsigned long)active->generation);
	}

	tmp_ = malloc(sizeof(*tmp_), M_TESSERA, M_WAITOK | M_ZERO);
	tmp_->devvp = devvp;
	tmp_->cp    = cp;
	tmp_->dev   = dev;
	tmp_->sb    = *active;
	/* `active` aliases sb_a or sb_b — copy completed, free the
	 * heap-alloc'd staging copies. */
	free(sb_a, M_TESSERA);
	free(sb_b, M_TESSERA);

	/* Wire the kmod block_io shim and open the inode tree against it.
	 * If the tree open fails (e.g. corrupted root sector) we still
	 * mount — the synthesized root vnode lets `df` / `umount` work
	 * for diagnostic purposes. */
	tmp_->bio_ctx.devvp = devvp;
	tmp_->bio_ctx.cred  = curthread->td_ucred;
	tmp_->bio_ctx.mount = tmp_;
	/* Data-zone io: alloc/free route through the extent allocator. */
	tmp_->bio.read_block  = tessera_kbio_read;
	tmp_->bio.write_block = tessera_kbio_write;
	tmp_->bio.alloc       = tessera_kbio_alloc;
	tmp_->bio.free        = tessera_kbio_free;
	tmp_->bio.ctx         = &tmp_->bio_ctx;
	/* Metadata-reserve io: alloc bumps sb.meta_reserve_bump. Used by
	 * inode_tree / pack_registry_tree COW puts and by extent_flush. */
	tmp_->meta_bio.read_block  = tessera_kbio_read;
	tmp_->meta_bio.write_block = tessera_kbio_write_delayed;
	tmp_->meta_bio.alloc       = tessera_kbio_meta_alloc;
	tmp_->meta_bio.free        = tessera_kbio_meta_free;
	tmp_->meta_bio.ctx         = &tmp_->bio_ctx;

	/* Meta-reserve recycler buffers. Sized to the entire reserve so
	 * we can never overflow on push. */
	tmp_->meta_free_cap     = (uint32_t)tmp_->sb.meta_reserve_length;
	tmp_->meta_pending_cap  = (uint32_t)tmp_->sb.meta_reserve_length;
	tmp_->meta_free    = malloc(tmp_->meta_free_cap    * sizeof(uint64_t),
	    M_TESSERA, M_WAITOK | M_ZERO);
	tmp_->meta_pending = malloc(tmp_->meta_pending_cap * sizeof(uint64_t),
	    M_TESSERA, M_WAITOK | M_ZERO);

	/* v2-step-2a: deferred-commit infrastructure. Initialise BEFORE
	 * mount-time GC runs — gc_data_zone calls commit_sb directly
	 * (synchronous, mount-time path), so the callout must be ready
	 * to be cancelled by unmount even if the FS never sees a write. */
	callout_init(&tmp_->flush_co, 1);
	TASK_INIT(&tmp_->flush_task, 0, tessera_fs_flush_task, tmp_);
	tmp_->flush_co_init = 1;

	/* Open the journal and run replay BEFORE we open the trees, so a
	 * rolled-forward generation lifts sb.inode_root / pack_registry_
	 * root / free_extent_root to the just-committed values. The
	 * trees then open against the correct (replayed) roots. */
	/* Forensic mounts skip journal replay: the journal sits at the
	 * live FS's frontier, and replaying its records would advance
	 * sb roots past the snapshot we requested. Read-only mounts
	 * also can't write the SB sectors, so the tail of the replay
	 * block (SB rewrite) would fail anyway. */
	tmp_->journal = (requested_gen != 0) ? NULL :
	    tessera_journal_open(&tmp_->bio,
	    tmp_->sb.journal_start, tmp_->sb.journal_length);
	if (tmp_->journal != NULL) {
		struct tessera_replay_ctx rctx = { .tmp_ = tmp_, .applied = 0 };
		(void)tessera_journal_replay(tmp_->journal,
		    tessera_replay_handler, &rctx);
		if (rctx.applied > 0) {
			/* Persist the rolled-forward SB so subsequent crashes
			 * see the recovered state without needing replay. */
			uint8_t *sbbuf = malloc(TESSERA_SECTOR_SIZE, M_TESSERA,
			    M_WAITOK | M_ZERO);
			if (tessera_encode_superblock(&tmp_->sb, sbbuf)
			    == TESSERA_OK) {
				(void)tessera_kbio_write(&tmp_->bio_ctx, 0, sbbuf);
				(void)tessera_kbio_write(&tmp_->bio_ctx, 1, sbbuf);
			}
			free(sbbuf, M_TESSERA);
			printf("tessera_fs: journal replay rolled forward "
			    "%d record(s); SB now gen=%lu\n",
			    rctx.applied,
			    (unsigned long)tmp_->sb.generation);
		}
	}

	/* Slice 2: tessera.gen=N — historical read-only mount.
	 *
	 * If the user passed `-o tessera.gen=N`, look up that snapshot
	 * record and substitute its (inode_root, pack_registry_root,
	 * free_extent_root) for the live ones. The live SB is left
	 * untouched on disk; this mount is just a read-only window onto
	 * gen N's tree set. MNT_RDONLY is enforced below so the kmod
	 * never writes back through the snapshot's roots — that would
	 * fork the timeline. */
	if (requested_gen != 0 && tmp_->sb.snapshots_root != 0) {
		tessera_btree_t *st = tessera_btree_open(&tmp_->meta_bio,
		    tmp_->sb.snapshots_root, /*tree_kind*/ 3,
		    /*key*/ 8, /*value*/ TESSERA_SNAPSHOT_RECORD_SIZE);
		if (st == NULL) {
			printf("tessera_fs: tessera.gen=%lu — snapshots tree "
			    "open failed\n", (unsigned long)requested_gen);
			free(tmp_->meta_free,    M_TESSERA);
			free(tmp_->meta_pending, M_TESSERA);
			free(tmp_, M_TESSERA);
			err = ENOENT;
			goto fail_close;
		}
		uint8_t skey[8];
		for (int i = 0; i < 8; i++)
			skey[i] = (uint8_t)(requested_gen >> ((7 - i) * 8));
		tessera_snapshot_record_t srec;
		if (tessera_btree_get(st, skey, &srec) != TESSERA_OK) {
			tessera_btree_close(st);
			printf("tessera_fs: tessera.gen=%lu — no such snapshot\n",
			    (unsigned long)requested_gen);
			free(tmp_->meta_free,    M_TESSERA);
			free(tmp_->meta_pending, M_TESSERA);
			free(tmp_, M_TESSERA);
			err = ENOENT;
			goto fail_close;
		}
		tessera_btree_close(st);

		printf("tessera_fs: historical mount at gen=%lu "
		    "(inode_root %lu, pack_root %lu, free_root %lu)\n",
		    (unsigned long)requested_gen,
		    (unsigned long)srec.inode_root,
		    (unsigned long)srec.pack_registry_root,
		    (unsigned long)srec.free_extent_root);

		tmp_->sb.generation         = srec.generation;
		tmp_->sb.inode_root         = srec.inode_root;
		tmp_->sb.pack_registry_root = srec.pack_registry_root;
		tmp_->sb.free_extent_root   = srec.free_extent_root;
		tmp_->readonly_snapshot     = 1;
		tmp_->snapshot_gen          = requested_gen;
	}

	tmp_->inode_tree = tessera_btree_open(&tmp_->meta_bio,
	    tmp_->sb.inode_root, /*tree_kind*/ 0,
	    /*key*/ 4, /*value*/ TESSERA_INODE_RECORD_SIZE);
	if (tmp_->inode_tree == NULL)
		printf("tessera_fs: warning — inode tree open at sector %lu "
		    "failed; root will be synthesized\n",
		    (unsigned long)tmp_->sb.inode_root);

	tmp_->pack_registry_tree = tessera_btree_open(&tmp_->meta_bio,
	    tmp_->sb.pack_registry_root, /*tree_kind*/ 1,
	    /*key*/ 16, /*value*/ TESSERA_REGISTRY_ENTRY_SIZE);
	if (tmp_->pack_registry_tree == NULL)
		printf("tessera_fs: warning — pack registry open at sector %lu "
		    "failed; blob lookups will fail\n",
		    (unsigned long)tmp_->sb.pack_registry_root);

	/* Snapshots tree (v2). Format slot was reserved in round 7-step9;
	 * v1 mkfs left it 0 ("not initialised"). On first mount with this
	 * kmod we lazy-allocate an empty tree; subsequent mounts open the
	 * existing one. tree_kind=3 is the snapshot kind; key is the
	 * 8-byte big-endian generation. Forensic mounts never lazy-
	 * allocate (would write to disk; we're read-only). */
	if (tmp_->sb.snapshots_root == 0 && !tmp_->readonly_snapshot) {
		uint64_t new_root = 0;
		tmp_->snapshots_tree = tessera_btree_create(&tmp_->meta_bio,
		    /*tree_kind*/ 3, /*key*/ 8,
		    /*value*/ TESSERA_SNAPSHOT_RECORD_SIZE, &new_root);
		if (tmp_->snapshots_tree != NULL && new_root != 0) {
			tmp_->sb.snapshots_root = new_root;
			tmp_->sb.snapshots_gen  = 1;
			/* SB write happens on the next commit_sb /
			 * unmount-flush. */
			tmp_->sb_dirty = 1;
			printf("tessera_fs: allocated empty snapshots tree "
			    "at sector %lu (v2 first-mount)\n",
			    (unsigned long)new_root);
		} else {
			printf("tessera_fs: warning — snapshots tree create "
			    "failed; time-machine disabled this mount\n");
		}
	} else {
		tmp_->snapshots_tree = tessera_btree_open(&tmp_->meta_bio,
		    tmp_->sb.snapshots_root, /*tree_kind*/ 3,
		    /*key*/ 8, /*value*/ TESSERA_SNAPSHOT_RECORD_SIZE);
		if (tmp_->snapshots_tree == NULL)
			printf("tessera_fs: warning — snapshots tree open at "
			    "sector %lu failed\n",
			    (unsigned long)tmp_->sb.snapshots_root);
	}

	/* Reconstruct the meta-reserve free list across mounts. The
	 * recycler's in-memory state is empty after open, so without a
	 * live-walk we'd push the bump pointer forward every mount-cycle
	 * even for unreferenced sectors. Strategy: walk every node of
	 * inode_tree, pack_registry_tree, AND the free-extent tree;
	 * mark referenced sectors. Anything in
	 * [meta_reserve_start, meta_reserve_bump) that is NOT marked is
	 * orphaned and safe to recycle. */
	{
		const uint64_t mstart = tmp_->sb.meta_reserve_start;
		const uint64_t mlen   = tmp_->sb.meta_reserve_length;
		const uint64_t mbump  = tmp_->sb.meta_reserve_bump;
		size_t bitmap_bytes = (size_t)((mlen + 7) / 8);
		uint8_t *bitmap = malloc(bitmap_bytes, M_TESSERA,
		    M_WAITOK | M_ZERO);
		struct meta_mark_ctx mctx = { bitmap, mstart, mstart + mlen };
		if (tmp_->inode_tree != NULL)
			(void)tessera_btree_walk_nodes(tmp_->inode_tree,
			    meta_mark_visitor, &mctx);
		if (tmp_->pack_registry_tree != NULL)
			(void)tessera_btree_walk_nodes(tmp_->pack_registry_tree,
			    meta_mark_visitor, &mctx);
		/* Free-extent tree: open separately, walk, close. The live
		 * extent_alloc handle (opened below) reads the same tree
		 * but doesn't expose its node sectors. */
		if (tmp_->sb.free_extent_root != 0) {
			tessera_btree_t *fet = tessera_btree_open(&tmp_->meta_bio,
			    tmp_->sb.free_extent_root, /*tree_kind*/ 2,
			    /*key*/ 8, /*value*/ 8);
			if (fet != NULL) {
				(void)tessera_btree_walk_nodes(fet,
				    meta_mark_visitor, &mctx);
				tessera_btree_close(fet);
			}
		}
		/* v2 snapshots: every retained snapshot's inode_tree,
		 * pack_registry_tree, and free_extent_tree node sectors are
		 * still live — we must NOT recycle their meta-reserve sectors
		 * even though the current SB doesn't reference them. COW
		 * sharing means most nodes are already covered by the walks
		 * above; this loop just plugs the remaining gap. */
		if (tmp_->snapshots_tree != NULL) {
			(void)tessera_btree_walk_nodes(tmp_->snapshots_tree,
			    meta_mark_visitor, &mctx);
			tessera_btree_cursor_t *sc =
			    tessera_btree_seek_first(tmp_->snapshots_tree);
			while (sc != NULL) {
				uint8_t sk[8];
				tessera_snapshot_record_t srec;
				if (tessera_btree_cursor_get(sc, sk, &srec)
				    != TESSERA_OK) break;
				if (srec.inode_root != 0 &&
				    srec.inode_root != tmp_->sb.inode_root) {
					tessera_btree_t *t =
					    tessera_btree_open(&tmp_->meta_bio,
					        srec.inode_root,
					        /*tree_kind*/ 0, /*key*/ 4,
					        /*value*/ TESSERA_INODE_RECORD_SIZE);
					if (t != NULL) {
						(void)tessera_btree_walk_nodes(
						    t, meta_mark_visitor, &mctx);
						tessera_btree_close(t);
					}
				}
				if (srec.pack_registry_root != 0 &&
				    srec.pack_registry_root !=
				        tmp_->sb.pack_registry_root) {
					tessera_btree_t *t =
					    tessera_btree_open(&tmp_->meta_bio,
					        srec.pack_registry_root,
					        /*tree_kind*/ 1, /*key*/ 16,
					        /*value*/ TESSERA_REGISTRY_ENTRY_SIZE);
					if (t != NULL) {
						(void)tessera_btree_walk_nodes(
						    t, meta_mark_visitor, &mctx);
						tessera_btree_close(t);
					}
				}
				if (srec.free_extent_root != 0 &&
				    srec.free_extent_root !=
				        tmp_->sb.free_extent_root) {
					tessera_btree_t *t =
					    tessera_btree_open(&tmp_->meta_bio,
					        srec.free_extent_root,
					        /*tree_kind*/ 2, /*key*/ 8,
					        /*value*/ 8);
					if (t != NULL) {
						(void)tessera_btree_walk_nodes(
						    t, meta_mark_visitor, &mctx);
						tessera_btree_close(t);
					}
				}
				if (tessera_btree_cursor_next(sc)
				    != TESSERA_OK) break;
			}
			if (sc != NULL) tessera_btree_cursor_free(sc);
		}
		/* Push every unmarked sector in the bumped range onto
		 * meta_free[]. Cap == mlen so push can't overflow. */
		uint32_t freed = 0;
		for (uint64_t s = mstart; s < mbump &&
		    tmp_->meta_free_count < tmp_->meta_free_cap; s++) {
			uint64_t bit = s - mstart;
			if (!(bitmap[bit / 8] & (1u << (bit % 8)))) {
				tmp_->meta_free[tmp_->meta_free_count++] = s;
				freed++;
			}
		}
		free(bitmap, M_TESSERA);
		if (freed > 0)
			printf("tessera_fs: meta-reserve reclaimed %u orphaned "
			    "sector(s) from prior session(s)\n", freed);
	}

	/* Open the free-extent allocator off the on-disk tree. Powers
	 * future kbio_alloc / _free calls (round 6+ mutation paths).
	 * For round 6a it's loaded but unused. */
	tmp_->extent_alloc = tessera_extent_open(&tmp_->bio,
	    tmp_->sb.free_extent_root);
	if (tmp_->extent_alloc == NULL)
		printf("tessera_fs: warning — free-extent open at sector %lu "
		    "failed; mutation paths will fail to allocate\n",
		    (unsigned long)tmp_->sb.free_extent_root);
	else
		printf("tessera_fs: extent allocator loaded — %lu free sectors, "
		    "largest run %lu\n",
		    (unsigned long)tessera_extent_free_blocks(tmp_->extent_alloc),
		    (unsigned long)tessera_extent_largest_free_run(tmp_->extent_alloc));

	/* Mount-time data-zone GC: reclaim packs whose only blob is no
	 * longer referenced by any live inode. Conservative — single-blob
	 * packs only; multi-blob mkfs-seeded packs are preserved. The
	 * earlier hang here was traced to two 4 KiB structs on the stack
	 * (sb_a/sb_b in mountfs and buf in commit_sb); both are now
	 * heap-allocated. */
	if (!tmp_->readonly_snapshot) {
		int gc_reclaimed = tessera_fs_gc_data_zone(tmp_);
		if (gc_reclaimed > 0)
			printf("tessera_fs: GC reclaimed %d orphaned pack(s); "
			    "%lu free sectors now\n", gc_reclaimed,
			    (unsigned long)tessera_extent_free_blocks(tmp_->extent_alloc));
	}

	mp->mnt_data = tmp_;
	mp->mnt_stat.f_namemax = TESSERA_PATH_NAME_MAX;
	mp->mnt_flag |= MNT_LOCAL;
	if (tmp_->readonly_snapshot)
		mp->mnt_flag |= MNT_RDONLY;
	/* MNT_RDONLY removed in round 6c — vop_setattr (utimes) is the
	 * first mutation. Other vop write-class ops still EOPNOTSUPP via
	 * default_vnodeops fallthrough. */
	MNT_ILOCK(mp);
	mp->mnt_kern_flag |= MNTK_LOOKUP_SHARED | MNTK_EXTENDED_SHARED;
	MNT_IUNLOCK(mp);

	if (devvp->v_rdev->si_iosize_max != 0)
		mp->mnt_iosize_max = devvp->v_rdev->si_iosize_max;
	if (mp->mnt_iosize_max > maxphys)
		mp->mnt_iosize_max = maxphys;

	(void)bo;
	printf("tessera_fs: mounted gen=%lu, %lu sectors\n",
	    (unsigned long)tmp_->sb.generation,
	    (unsigned long)tmp_->sb.total_sectors);
	return (0);

fail_close:
	if (sb_a) free(sb_a, M_TESSERA);
	if (sb_b) free(sb_b, M_TESSERA);
	g_topology_lock();
	g_vfs_close(cp);
	g_topology_unlock();
	dev_rel(dev);
	return (err);
}

/* ── vfs_mount ───────────────────────────────────────────────── */

static int
tessera_mount_impl(struct mount *mp)
{
	struct vnode *devvp;
	struct nameidata ndp;
	struct thread *td = curthread;
	char *fspec;
	int err;

	if (mp->mnt_flag & MNT_UPDATE)
		return (EOPNOTSUPP);

	fspec = vfs_getopts(mp->mnt_optnew, "from", &err);
	if (err != 0) return (err);

	/* Slice 2: optional `tessera.gen=<u64>` mount option requests a
	 * read-only historical view at that snapshot generation. Parse
	 * the string here; mountfs interprets it. */
	uint64_t requested_gen = 0;
	{
		int gen_err;
		char *gen_str = vfs_getopts(mp->mnt_optnew, "tessera.gen",
		    &gen_err);
		if (gen_err == 0 && gen_str != NULL) {
			uint64_t v = 0;
			for (const char *p = gen_str; *p; p++) {
				if (*p < '0' || *p > '9') { v = 0; break; }
				v = v * 10 + (uint64_t)(*p - '0');
			}
			requested_gen = v;
		}
	}

	NDINIT(&ndp, LOOKUP, FOLLOW | LOCKLEAF, UIO_SYSSPACE, fspec);
	err = namei(&ndp);
	if (err != 0) return (err);
	NDFREE_PNBUF(&ndp);
	devvp = ndp.ni_vp;

	if (!vn_isdisk_error(devvp, &err)) {
		vput(devvp);
		return (err);
	}

	err = VOP_ACCESS(devvp, VREAD, td->td_ucred, td);
	if (err != 0)
		err = priv_check(td, PRIV_VFS_MOUNT_PERM);
	if (err != 0) {
		vput(devvp);
		return (err);
	}

	err = tessera_mountfs(devvp, mp, requested_gen);
	if (err != 0) {
		vrele(devvp);
		return (err);
	}
	vfs_mountedfrom(mp, fspec);
	return (0);
}

/* ── vfs_unmount ─────────────────────────────────────────────── */

static int
tessera_unmount_impl(struct mount *mp, int mntflags)
{
	struct tessera_mount *tmp_ = VFSTOTESSERA(mp);
	int flags = 0, err;

	if (mntflags & MNT_FORCE) flags |= FORCECLOSE;
	err = vflush(mp, 0, flags, curthread);
	if (err != 0) return (err);

	if (tmp_ != NULL) {
		/* v2-step-2a: stop accepting new flushes, drain any in-flight
		 * callout / task, then do one final synchronous flush so the
		 * SB on disk reflects every committed mutation before tear-
		 * down. After this point sb_dirty must be 0. */
		if (tmp_->flush_co_init) {
			tmp_->flush_unmounting = 1;
			callout_drain(&tmp_->flush_co);
			taskqueue_drain(taskqueue_thread, &tmp_->flush_task);
			(void)tessera_fs_flush(tmp_);
		}
		if (tmp_->journal != NULL)
			tessera_journal_close(tmp_->journal);
		if (tmp_->extent_alloc != NULL)
			tessera_extent_close(tmp_->extent_alloc);
		if (tmp_->snapshots_tree != NULL)
			tessera_btree_close(tmp_->snapshots_tree);
		if (tmp_->pack_registry_tree != NULL)
			tessera_btree_close(tmp_->pack_registry_tree);
		if (tmp_->inode_tree != NULL)
			tessera_btree_close(tmp_->inode_tree);
		if (tmp_->meta_free    != NULL) free(tmp_->meta_free, M_TESSERA);
		if (tmp_->meta_pending != NULL) free(tmp_->meta_pending, M_TESSERA);
		if (tmp_->cp != NULL) {
			g_topology_lock();
			g_vfs_close(tmp_->cp);
			g_topology_unlock();
		}
		if (tmp_->devvp != NULL) vrele(tmp_->devvp);
		if (tmp_->dev != NULL)   dev_rel(tmp_->dev);
		free(tmp_, M_TESSERA);
		mp->mnt_data = NULL;
	}
	return (0);
}

/* ── vfs_root ────────────────────────────────────────────────── */

/*
 * vnode dedup via vfs_hash. Without this, every vop_lookup descent
 * AND every "ISDOTDOT" allocation produces a fresh vnode for the
 * same on-disk inode — fts(3) compares vnode identity across walk
 * steps and prints "fts_read: No such file or directory" when it
 * sees the duplicates (output is still correct, but stderr noise).
 *
 * Helper: tessera_vget(mp, inode_no, parent_inode_no, **vpp)
 *   - Locks and returns an existing vnode if one is already in
 *     vfs_hash for inode_no.
 *   - Otherwise allocates one, reads the on-disk inode record to
 *     set v_type, vfs_hash_inserts. Loses-race case: discards our
 *     vnode and returns the winner.
 */
static int
tessera_inode_cmp(struct vnode *vp, void *arg)
{
	uint64_t target = *(uint64_t *)arg;
	struct tessera_node *tn = VTOTNODE(vp);
	if (tn == NULL) return (1);
	return (tn->inode_no == target) ? 0 : 1;
}

static int
tessera_vget(struct mount *mp, uint64_t inode_no, uint64_t parent_inode_no,
             struct vnode **vpp)
{
	struct vnode *vp = NULL;
	struct thread *td = curthread;
	uint64_t key = inode_no;
	int error;

	error = vfs_hash_get(mp, (u_int)inode_no, LK_EXCLUSIVE, td, &vp,
	    tessera_inode_cmp, &key);
	if (error != 0) return (error);
	if (vp != NULL) {
		*vpp = vp;
		return (0);
	}

	error = getnewvnode("tessera", mp, &tessera_vnodeops, &vp);
	if (error != 0) return (error);
	vn_lock(vp, LK_EXCLUSIVE | LK_RETRY);

	struct tessera_node *tn = malloc(sizeof(*tn), M_TESSERA,
	    M_WAITOK | M_ZERO);
	tn->inode_no        = inode_no;
	tn->parent_inode_no = parent_inode_no;
	vp->v_data = tn;
	vp->v_type = VNON;

	struct tessera_mount *tmp_ = VFSTOTESSERA(mp);
	if (tmp_->inode_tree != NULL) {
		uint8_t k4[4];
		tessera_inode_record_t cino;
		encode_inode_key((uint32_t)inode_no, k4);
		if (tessera_btree_get(tmp_->inode_tree, k4, &cino)
		    == TESSERA_OK) {
			switch (cino.mode & 0170000) {
			case 0040000: vp->v_type = VDIR; break;
			case 0100000: vp->v_type = VREG; break;
			case 0120000: vp->v_type = VLNK; break;
			default:      vp->v_type = VBAD; break;
			}
		}
	}
	if (inode_no == TESSERA_INODE_ROOT_DIR) {
		vp->v_type = VDIR;
		vp->v_vflag |= VV_ROOT;
	}

	VN_LOCK_ASHARE(vp);
	if (insmntque1(vp, mp) != 0) {
		vp->v_data = NULL;
		vp->v_op = &dead_vnodeops;
		vgone(vp);
		vput(vp);
		free(tn, M_TESSERA);
		return (EIO);
	}

	struct vnode *other = NULL;
	error = vfs_hash_insert(vp, (u_int)inode_no, LK_EXCLUSIVE, td, &other,
	    tessera_inode_cmp, &key);
	if (error != 0) {
		vput(vp);
		return (error);
	}
	if (other != NULL) {
		/* Race: another thread won the slot. Discard ours. */
		vgone(vp);
		vput(vp);
		*vpp = other;
		return (0);
	}

	vn_set_state(vp, VSTATE_CONSTRUCTED);
	*vpp = vp;
	return (0);
}

static int
tessera_root_impl(struct mount *mp, int flags, struct vnode **vpp)
{
	(void)flags;
	return (tessera_vget(mp, TESSERA_INODE_ROOT_DIR,
	    /* "/.." == "/" */ TESSERA_INODE_ROOT_DIR, vpp));
}

/* ── vfs_statfs ──────────────────────────────────────────────── */

static int
tessera_statfs_impl(struct mount *mp, struct statfs *sbp)
{
	struct tessera_mount *tmp_ = VFSTOTESSERA(mp);

	sbp->f_bsize  = TESSERA_SECTOR_SIZE;
	sbp->f_iosize = TESSERA_SECTOR_SIZE;
	sbp->f_blocks = tmp_->sb.total_sectors;
	/* Live free count from the data-zone extent allocator (the
	 * authoritative in-memory state). Falls back to the static
	 * pack_zone_length if the allocator failed to load. */
	uint64_t free_data = (tmp_->extent_alloc != NULL)
	    ? tessera_extent_free_blocks(tmp_->extent_alloc)
	    : tmp_->sb.pack_zone_length;
	sbp->f_bfree  = free_data;
	sbp->f_bavail = free_data;
	sbp->f_files  = 0;
	sbp->f_ffree  = 0;
	return (0);
}

/* ── vfs_init / uninit ───────────────────────────────────────── */

static int
tessera_init_impl(struct vfsconf *vfsp)
{
	(void)vfsp;
	printf("tessera_fs: VFS registered\n");
	return (0);
}

static int
tessera_uninit_impl(struct vfsconf *vfsp)
{
	(void)vfsp;
	printf("tessera_fs: VFS unregistered\n");
	return (0);
}

static struct vfsops tessera_vfsops = {
	.vfs_mount   = tessera_mount_impl,
	.vfs_unmount = tessera_unmount_impl,
	.vfs_root    = tessera_root_impl,
	.vfs_statfs  = tessera_statfs_impl,
	.vfs_init    = tessera_init_impl,
	.vfs_uninit  = tessera_uninit_impl,
	.vfs_sync    = vfs_stdsync,
};

/* ── vop ops on the synthesized root ─────────────────────────── */

static int
tessera_vop_access(struct vop_access_args *ap)
{
	struct vnode *vp = ap->a_vp;
	struct tessera_node *tn = VTOTNODE(vp);
	struct tessera_mount *tmp_ = VFSTOTESSERA(vp->v_mount);

	if (tmp_->inode_tree == NULL) {
		/* Pre-tree-open synthesized root: permissive. */
		return (0);
	}
	uint8_t key[4];
	tessera_inode_record_t ino;
	encode_inode_key((uint32_t)tn->inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &ino) != TESSERA_OK)
		return (EIO);
	/* vaccess() does the standard POSIX permission-bit check against
	 * cred (root override, owner/group/other). */
	return (vaccess(vp->v_type, ino.mode & 07777, ino.uid, ino.gid,
	    ap->a_accmode, ap->a_cred));
}

static void
encode_inode_key(uint32_t inode_no, uint8_t out[4])
{
	/* Big-endian — makes the B+tree's memcmp ordering match numeric
	 * ordering of inode numbers. */
	out[0] = (uint8_t)(inode_no >> 24);
	out[1] = (uint8_t)(inode_no >> 16);
	out[2] = (uint8_t)(inode_no >>  8);
	out[3] = (uint8_t)(inode_no      );
}

static int
tessera_vop_getattr(struct vop_getattr_args *ap)
{
	struct vnode *vp = ap->a_vp;
	struct vattr *vap = ap->a_vap;
	struct tessera_node *tn = VTOTNODE(vp);
	struct tessera_mount *tmp_ = VFSTOTESSERA(vp->v_mount);

	VATTR_NULL(vap);
	vap->va_fsid      = vp->v_mount->mnt_stat.f_fsid.val[0];
	vap->va_fileid    = tn->inode_no;
	vap->va_blocksize = TESSERA_SECTOR_SIZE;

	/* Try to read the on-disk inode record. ENOENT fall-through means
	 * the volume hasn't had inode 2 populated yet (mkfs work pending
	 * for round 3c) — return synthesized empty-root attrs so the
	 * mount stays usable for diagnostic stat/df. */
	int read_real = 0;
	if (tmp_->inode_tree != NULL) {
		uint8_t key[4];
		tessera_inode_record_t ino;
		encode_inode_key((uint32_t)tn->inode_no, key);
		int rc = tessera_btree_get(tmp_->inode_tree, key, &ino);
		if (rc == TESSERA_OK) {
			switch (ino.mode & 0170000) {
			case 040000:  vap->va_type = VDIR; break;
			case 0100000: vap->va_type = VREG; break;
			case 0120000: vap->va_type = VLNK; break;
			default:      vap->va_type = VBAD; break;
			}
			vap->va_mode  = ino.mode & 07777;
			vap->va_nlink = ino.nlink ? ino.nlink : 2;
			vap->va_uid   = ino.uid;
			vap->va_gid   = ino.gid;
			vap->va_size  = ino.size;
			vap->va_atime.tv_sec  =  ino.atime_ns / 1000000000ULL;
			vap->va_atime.tv_nsec = ino.atime_ns % 1000000000ULL;
			vap->va_mtime.tv_sec  =  ino.mtime_ns / 1000000000ULL;
			vap->va_mtime.tv_nsec = ino.mtime_ns % 1000000000ULL;
			vap->va_ctime.tv_sec  =  ino.ctime_ns / 1000000000ULL;
			vap->va_ctime.tv_nsec = ino.ctime_ns % 1000000000ULL;
			vap->va_birthtime.tv_sec  = ino.btime_ns / 1000000000ULL;
			vap->va_birthtime.tv_nsec = ino.btime_ns % 1000000000ULL;
			vap->va_gen   = ino.gen ? ino.gen : 1;
			vap->va_flags = ino.flags;
			vap->va_bytes = 0;
			read_real = 1;
		}
	}
	if (!read_real) {
		vap->va_type  = VDIR;
		vap->va_mode  = 0755;
		vap->va_nlink = 2;
		vap->va_uid   = 0;
		vap->va_gid   = 0;
		vap->va_size  = 0;
		vap->va_bytes = 0;
		vap->va_gen   = 1;
		vap->va_flags = 0;
	}
	return (0);
}

/*
 * Walk a parsed DIRECTORY manifest looking for `name` (length nlen).
 * On hit, *out_inode is the child inode_no. The body is the encoded
 * dirent stream from manifest.c: for each entry,
 *   uint64 child_inode | uint16 name_len | name[name_len]
 * Sorted by name (memcmp) so a binary search would work; for this
 * round we linear-scan since directories are small in our test
 * images.
 */
static int
tessera_dir_lookup_name(const uint8_t *body, size_t body_len,
                        const char *name, uint16_t nlen,
                        uint64_t *out_inode)
{
	size_t off = 0;
	while (off + 10 <= body_len) {
		uint64_t child;
		uint16_t entry_nlen;
		memcpy(&child,       body + off,     8);
		memcpy(&entry_nlen,  body + off + 8, 2);
		if (off + 10 + entry_nlen > body_len) return (EIO);
		if (entry_nlen == nlen &&
		    memcmp(body + off + 10, name, nlen) == 0) {
			*out_inode = child;
			return (0);
		}
		off += 10 + entry_nlen;
	}
	return (ENOENT);
}

static int
tessera_vop_lookup(struct vop_lookup_args *ap)
{
	struct vnode *dvp = ap->a_dvp;
	struct vnode **vpp = ap->a_vpp;
	struct componentname *cnp = ap->a_cnp;
	struct tessera_mount *tmp_ = VFSTOTESSERA(dvp->v_mount);
	struct tessera_node *dn = VTOTNODE(dvp);

	*vpp = NULL;
	if (cnp->cn_namelen == 1 && cnp->cn_nameptr[0] == '.') {
		vref(dvp);
		*vpp = dvp;
		return (0);
	}
	if (cnp->cn_flags & ISDOTDOT) {
		uint64_t parent = dn->parent_inode_no;
		if (parent == 0 || parent == dn->inode_no) {
			/* Root or unknown — return self per FS convention. */
			vref(dvp);
			*vpp = dvp;
			return (0);
		}
		/* FreeBSD ".." protocol: drop dvp's lock around the
		 * parent vget to avoid dvp→parent lock inversion (parent
		 * vnode may be the same vnode another walker holds). */
		struct vnode *pvp;
		VOP_UNLOCK(dvp);
		int e = tessera_vget(dvp->v_mount, parent,
		    /*grandparent unknown*/ 0, &pvp);
		vn_lock(dvp, LK_EXCLUSIVE | LK_RETRY);
		if (e != 0) return (e);
		*vpp = pvp;
		return (0);
	}

	/* Real on-disk lookup: read the directory inode, fetch its
	 * DIRECTORY manifest blob, walk it for `cnp`. */
	if (tmp_->inode_tree == NULL) return (ENOENT);

	uint8_t key[4];
	tessera_inode_record_t dino;
	encode_inode_key((uint32_t)dn->inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &dino) != TESSERA_OK)
		return (ENOENT);
	if (tessera_hash_is_null(dino.manifest_hash))
		return (ENOENT);                  /* empty directory */

	uint8_t *blob = NULL;
	uint32_t blob_len = 0;
	int err = tessera_fs_fetch_blob(tmp_, dino.manifest_hash,
	    &blob, &blob_len);
	if (err != 0) return (ENOENT);

	tessera_manifest_parser_t *p = tessera_manifest_parse(blob, blob_len);
	if (p == NULL) {
		free(blob, M_TESSERA);
		return (EIO);
	}
	if (tessera_manifest_parser_kind(p) != TESSERA_MFT_DIRECTORY) {
		tessera_manifest_parser_free(p);
		free(blob, M_TESSERA);
		return (ENOTDIR);
	}

	/* manifest body is the parser's internal slice — recover it via
	 * inline_data accessor (works for INLINE / SYMLINK; for DIRECTORY
	 * we cheat and walk the buffer past the 32-byte header directly). */
	const uint8_t *body = blob + 32;
	const size_t   blen = blob_len - 32;
	uint64_t child_no = 0;
	int rc = tessera_dir_lookup_name(body, blen,
	    cnp->cn_nameptr, (uint16_t)cnp->cn_namelen, &child_no);
	tessera_manifest_parser_free(p);
	free(blob, M_TESSERA);
	if (rc != 0) {
		/* Standard FreeBSD lookup convention: on the final path
		 * component, when the caller is about to CREATE / RENAME
		 * this name, return EJUSTRETURN so namei keeps the parent
		 * locked and routes the next op (VOP_CREATE etc.) here. */
		if ((cnp->cn_flags & ISLASTCN) &&
		    (cnp->cn_nameiop == CREATE || cnp->cn_nameiop == RENAME))
			return (EJUSTRETURN);
		return (rc);
	}

	/* Found — return a deduped vnode. tessera_vget reads the on-disk
	 * inode record and sets v_type; if the vnode already exists in
	 * the kernel's vfs_hash, the cached one is returned (avoids the
	 * fts_read warning that came from minting fresh vnodes per
	 * lookup). */
	return (tessera_vget(dvp->v_mount, child_no, dn->inode_no, vpp));
}

#define DIRENT_HDR  offsetof(struct dirent, d_name)

static size_t
tessera_dirent_reclen(uint16_t namlen)
{
	size_t need = DIRENT_HDR + namlen + 1;
	return ((need + 7) & ~7);
}

static int
tessera_emit_dirent(struct uio *uio, ino_t fileno, uint8_t type,
                    const char *name, uint16_t namlen)
{
	/* 280 covers any TESSERA_PATH_NAME_MAX (255) plus header + NUL +
	 * 8-byte alignment slack. */
	uint8_t buf[288];
	size_t reclen = tessera_dirent_reclen(namlen);
	if (reclen > sizeof(buf)) return (ENAMETOOLONG);

	struct dirent *de = (struct dirent *)buf;
	bzero(buf, reclen);
	de->d_fileno = fileno;
	de->d_off    = uio->uio_offset + reclen;
	de->d_reclen = (uint16_t)reclen;
	de->d_type   = type;
	de->d_namlen = namlen;
	memcpy(de->d_name, name, namlen);
	return (uiomove(buf, reclen, uio));
}

static uint8_t
tessera_dt_from_mode(uint32_t mode)
{
	switch (mode & 0170000) {
	case 040000:  return (DT_DIR);
	case 0100000: return (DT_REG);
	case 0120000: return (DT_LNK);
	default:      return (DT_UNKNOWN);
	}
}

static int
tessera_vop_readdir(struct vop_readdir_args *ap)
{
	struct uio   *uio = ap->a_uio;
	struct tessera_node *tn = VTOTNODE(ap->a_vp);
	struct tessera_mount *tmp_ = VFSTOTESSERA(ap->a_vp->v_mount);
	int err = 0;

	/* Round 4d strategy: emit the entire directory in the first
	 * call. Subsequent calls (uio_offset > 0) return EOF. Correct
	 * for ls(1)-shape consumers with a 4 KiB buffer; large
	 * directories will need an offset-cookie scheme in round 5+. */
	if (uio->uio_offset > 0) {
		if (ap->a_eofflag != NULL) *ap->a_eofflag = 1;
		return (0);
	}

	/* Always emit "." and "..". */
	err = tessera_emit_dirent(uio, tn->inode_no, DT_DIR, ".", 1);
	if (err != 0) return (err);
	err = tessera_emit_dirent(uio, tn->inode_no, DT_DIR, "..", 2);
	if (err != 0) return (err);

	/* Walk the directory's manifest, if any. */
	if (tmp_->inode_tree == NULL) goto done;

	uint8_t key[4];
	tessera_inode_record_t dino;
	encode_inode_key((uint32_t)tn->inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &dino) != TESSERA_OK)
		goto done;
	if (tessera_hash_is_null(dino.manifest_hash))
		goto done;

	uint8_t *blob = NULL;
	uint32_t blob_len = 0;
	if (tessera_fs_fetch_blob(tmp_, dino.manifest_hash, &blob, &blob_len)
	    != 0)
		goto done;

	tessera_manifest_parser_t *p = tessera_manifest_parse(blob, blob_len);
	if (p == NULL) {
		free(blob, M_TESSERA);
		goto done;
	}
	if (tessera_manifest_parser_kind(p) != TESSERA_MFT_DIRECTORY) {
		tessera_manifest_parser_free(p);
		free(blob, M_TESSERA);
		goto done;
	}
	const uint8_t *body = blob + 32;
	const size_t   blen = blob_len - 32;

	for (size_t off = 0; off + 10 <= blen; ) {
		uint64_t child;
		uint16_t name_len;
		memcpy(&child,    body + off,     8);
		memcpy(&name_len, body + off + 8, 2);
		if (off + 10 + name_len > blen) break;
		const char *name = (const char *)(body + off + 10);

		uint8_t dt = DT_UNKNOWN;
		uint8_t k2[4];
		tessera_inode_record_t cino;
		encode_inode_key((uint32_t)child, k2);
		if (tessera_btree_get(tmp_->inode_tree, k2, &cino) == TESSERA_OK)
			dt = tessera_dt_from_mode(cino.mode);

		err = tessera_emit_dirent(uio, child, dt, name, name_len);
		if (err != 0) {
			tessera_manifest_parser_free(p);
			free(blob, M_TESSERA);
			return (err);
		}
		off += 10 + name_len;
	}
	tessera_manifest_parser_free(p);
	free(blob, M_TESSERA);

done:
	if (ap->a_eofflag != NULL) *ap->a_eofflag = 1;
	return (0);
}

static int
tessera_vop_read(struct vop_read_args *ap)
{
	struct vnode *vp = ap->a_vp;
	struct uio   *uio = ap->a_uio;
	struct tessera_node *tn = VTOTNODE(vp);
	struct tessera_mount *tmp_ = VFSTOTESSERA(vp->v_mount);
	int err = 0;

	if (vp->v_type == VDIR) return (EISDIR);
	if (vp->v_type != VREG && vp->v_type != VNON) return (EINVAL);
	if (uio->uio_offset < 0) return (EINVAL);
	if (uio->uio_resid == 0) return (0);
	if (tmp_->inode_tree == NULL) return (EIO);

	/* Read the inode record. */
	uint8_t key[4];
	tessera_inode_record_t ino;
	encode_inode_key((uint32_t)tn->inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &ino) != TESSERA_OK)
		return (EIO);

	/* Empty file or read past EOF. */
	if ((uint64_t)uio->uio_offset >= ino.size) return (0);
	if (tessera_hash_is_null(ino.manifest_hash)) return (0);

	/* Fetch + parse the manifest. */
	uint8_t *blob = NULL;
	uint32_t blob_len = 0;
	if (tessera_fs_fetch_blob(tmp_, ino.manifest_hash, &blob, &blob_len)
	    != 0)
		return (EIO);
	tessera_manifest_parser_t *p = tessera_manifest_parse(blob, blob_len);
	if (p == NULL) {
		free(blob, M_TESSERA);
		return (EIO);
	}

	const tessera_manifest_kind_t kind =
	    tessera_manifest_parser_kind(p);
	if (kind == TESSERA_MFT_INLINE) {
		const uint8_t *data = NULL;
		size_t data_len = 0;
		if (tessera_manifest_inline_data(p, &data, &data_len)
		    != TESSERA_OK || data == NULL) {
			err = EIO;
			goto out;
		}
		/* Clamp to inode.size in case the manifest is longer. */
		if (data_len > ino.size) data_len = (size_t)ino.size;
		if ((uint64_t)uio->uio_offset >= data_len) goto out;

		size_t remaining = data_len - (size_t)uio->uio_offset;
		size_t n = (uio->uio_resid < (ssize_t)remaining)
		    ? (size_t)uio->uio_resid : remaining;
		err = uiomove(__DECONST(void *, data + uio->uio_offset),
		    n, uio);
	} else if (kind == TESSERA_MFT_CHUNK_LIST) {
		const uint32_t n = tessera_manifest_parser_count(p);
		for (uint32_t i = 0; i < n && uio->uio_resid > 0; i++) {
			tessera_chunk_record_t cr;
			if (tessera_manifest_chunk_at(p, i, &cr)
			    != TESSERA_OK) {
				err = EIO;
				break;
			}
			const uint64_t cstart = cr.logical_offset;
			const uint64_t cend   = cstart + cr.uncompressed_size;

			/* Skip chunks entirely below the read window. */
			if (cend <= (uint64_t)uio->uio_offset) continue;
			/* Stop when we've passed the read window. */
			if (cstart >= (uint64_t)uio->uio_offset
			    + (uint64_t)uio->uio_resid) break;

			/* Slice intersection of [cstart, cend) and
			 * [uio_offset, uio_offset + resid). */
			const uint64_t lo = ((uint64_t)uio->uio_offset > cstart)
			    ? (uint64_t)uio->uio_offset - cstart : 0;
			const uint64_t hi_off =
			    (uint64_t)uio->uio_offset + (uint64_t)uio->uio_resid;
			const uint64_t hi = (hi_off < cend
			    ? hi_off - cstart : cr.uncompressed_size);
			const size_t   n_copy = (size_t)(hi - lo);

			if (cr.flags & TESSERA_CHUNK_FLAG_ZERO_HOLE) {
				/* Sparse hole: synthesize zeros, no fetch. */
				uint8_t *zb = malloc(n_copy, M_TESSERA,
				    M_WAITOK | M_ZERO);
				err = uiomove(zb, n_copy, uio);
				free(zb, M_TESSERA);
				if (err != 0) break;
				continue;
			}

			uint8_t *cb = NULL;
			uint32_t cb_len = 0;
			if (tessera_fs_fetch_blob(tmp_, cr.chunk_hash,
			    &cb, &cb_len) != 0) {
				err = EIO;
				break;
			}
			if (cb_len < cr.uncompressed_size) {
				free(cb, M_TESSERA);
				err = EIO;
				break;
			}

			err = uiomove(cb + lo, n_copy, uio);
			free(cb, M_TESSERA);
			if (err != 0) break;
		}
	} else if (kind == TESSERA_MFT_CHUNK_TREE) {
		err = EOPNOTSUPP;     /* round 5c */
	} else {
		err = EIO;
	}
out:
	tessera_manifest_parser_free(p);
	free(blob, M_TESSERA);
	return (err);
}

/* ── vop_setattr (utimes / chmod / chown / chflags) ─────────── */

/*
 * vop_setattr — utimes only (chmod/chown/truncate still EOPNOTSUPP).
 *
 * Path: btree_get (current inode record) → patch atime/mtime/ctime →
 * btree_put (COW path through meta_bio, allocates a new node sector
 * from the metadata reserve, returns the new tree root) → commit_sb
 * (synchronous SB write to sectors 0+1 with the new inode_root and
 * bumped generation, so the change persists across umount/remount).
 *
 * The "deadlock during touch" investigated through round 6c-redux
 * was actually a kernel stack overflow in btree_put (4 KiB stack
 * arrays per recursion level vs FreeBSD aarch64's 16 KiB kstack).
 * Fix: btree.c put-path now heap-allocates its node buffers.
 */
static int
tessera_vop_setattr(struct vop_setattr_args *ap)
{
	struct vnode *vp = ap->a_vp;
	struct vattr *vap = ap->a_vap;
	struct tessera_node *tn = VTOTNODE(vp);
	struct tessera_mount *tmp_ = VFSTOTESSERA(vp->v_mount);

	int seen_atime = (vap->va_atime.tv_sec != VNOVAL);
	int seen_mtime = (vap->va_mtime.tv_sec != VNOVAL);
	int seen_size  = (vap->va_size != (u_quad_t)VNOVAL);
	int seen_mode  = (vap->va_mode != (mode_t)VNOVAL);
	int seen_uid   = (vap->va_uid  != (uid_t)VNOVAL);
	int seen_gid   = (vap->va_gid  != (gid_t)VNOVAL);

	if (!seen_atime && !seen_mtime && !seen_size && !seen_mode &&
	    !seen_uid && !seen_gid)
		return (0);
	if (tmp_->inode_tree == NULL) return (EROFS);

	uint8_t key[4];
	tessera_inode_record_t ino;
	encode_inode_key((uint32_t)tn->inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &ino) != TESSERA_OK)
		return (EIO);

	/* Truncate / extend (handles `>` shell redirection's pre-write
	 * VOP_SETATTR(va_size = 0)). The new content is built in RAM by
	 * reading the existing one (capped at min(old_size, new_size))
	 * and zero-padding any extension. v1 keeps everything in RAM and
	 * always republishes as INLINE — fine for small files, will be
	 * replaced with chunked writes once vop_write goes chunked. */
	int did_resize = 0;
	if (seen_size) {
		uint64_t new_size = (uint64_t)vap->va_size;
		if (new_size != ino.size) {
			uint8_t *old_buf = NULL;
			size_t   old_len = 0;
			if (tessera_fs_read_full_content(tmp_, &ino,
			    &old_buf, &old_len) != 0)
				return (EIO);
			uint8_t *new_buf = malloc((size_t)new_size, M_TESSERA,
			    M_WAITOK | M_ZERO);
			size_t copy_len = old_len < (size_t)new_size
			    ? old_len : (size_t)new_size;
			if (copy_len > 0 && old_buf != NULL)
				memcpy(new_buf, old_buf, copy_len);
			if (old_buf) free(old_buf, M_TESSERA);
			int rc = tessera_fs_replace_content(tmp_,
			    (uint32_t)tn->inode_no, new_buf, (size_t)new_size);
			free(new_buf, M_TESSERA);
			if (rc != 0) return (rc);
			/* replace_content rewrote the inode record; re-fetch
			 * the live copy so atime/mtime updates below stick. */
			if (tessera_btree_get(tmp_->inode_tree, key, &ino)
			    != TESSERA_OK) return (EIO);
			did_resize = 1;
		}
	}

	if (seen_atime)
		ino.atime_ns = (uint64_t)vap->va_atime.tv_sec * 1000000000ULL +
		    (uint64_t)vap->va_atime.tv_nsec;
	if (seen_mtime)
		ino.mtime_ns = (uint64_t)vap->va_mtime.tv_sec * 1000000000ULL +
		    (uint64_t)vap->va_mtime.tv_nsec;
	if (seen_mode)
		/* keep S_IFMT bits, replace permission bits */
		ino.mode = (ino.mode & 0170000) | (vap->va_mode & 07777);
	if (seen_uid) ino.uid = vap->va_uid;
	if (seen_gid) ino.gid = vap->va_gid;
	if (seen_atime || seen_mtime || seen_mode || seen_uid || seen_gid) {
		struct timeval tv;
		getmicrotime(&tv);
		ino.ctime_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
		    (uint64_t)tv.tv_usec * 1000ULL;
		uint64_t new_root = tmp_->sb.inode_root;
		if (tessera_btree_put(tmp_->inode_tree, key, &ino,
		    &new_root) != TESSERA_OK)
			return (EIO);
		tmp_->sb.inode_root = new_root;
	}

	if (did_resize) {
		if (tessera_commit_extent(tmp_) != 0)
			return (EIO);
	}
	tessera_fs_mark_dirty(tmp_);
	return (0);
}

static int
tessera_vop_open(struct vop_open_args *ap)
{ (void)ap; return (0); }

static int
tessera_vop_close(struct vop_close_args *ap)
{ (void)ap; return (0); }

/* v2-step-2a: mutations now defer the SB commit. vop_fsync forces the
 * deferred commit so the just-written data is durable on return. Per-
 * inode fsync rather than per-mount group commit — Phase 2-polish will
 * add jbd2-style group commit. */
static int
tessera_vop_fsync(struct vop_fsync_args *ap)
{
	struct tessera_mount *tmp_ = VFSTOTESSERA(ap->a_vp->v_mount);
	if (tmp_ == NULL) return (0);
	return (tessera_fs_flush(tmp_) == 0 ? 0 : EIO);
}

static int
tessera_vop_reclaim(struct vop_reclaim_args *ap)
{
	struct vnode *vp = ap->a_vp;
	struct tessera_node *tn = VTOTNODE(vp);

	/* Remove from vfs_hash before freeing tessera_node — the cmp
	 * callback dereferences VTOTNODE. */
	vfs_hash_remove(vp);
	if (tn != NULL) {
		free(tn, M_TESSERA);
		vp->v_data = NULL;
	}
	return (0);
}

/* ── transaction commit (Round 6c, MVP) ──────────────────────── */

/*
 * Body of the ROOT_UPDATE journal record: 32-byte snapshot of the
 * four advancing roots. Written by tessera_commit_sb and consumed by
 * tessera_replay_handler on the next mount.
 */
struct tessera_jrec_sb_commit {
	uint64_t generation;
	uint64_t inode_root;
	uint64_t pack_registry_root;
	uint64_t free_extent_root;
};

/*
 * Write the in-memory superblock back to sectors 0 and 1 with the
 * generation bumped. Each commit is preceded by a journal transaction
 * (BEGIN → ROOT_UPDATE record → COMMIT). On remount,
 * tessera_replay_handler re-applies any record whose generation
 * exceeds the on-disk SB's — this rolls forward a crash that landed
 * between tx_commit and the SB sector writes.
 */
static int
tessera_commit_sb(struct tessera_mount *tmp_)
{
	tessera_stat_sb_commits++;
	tmp_->sb.generation++;

	/* v2 snapshots: append a record for the about-to-commit gen
	 * BEFORE writing the SB. The record references the same roots
	 * the SB is about to commit, plus the just-incremented gen. The
	 * snapshots_root advances; that new root then ends up in the SB
	 * sector we write below, so the SB references a snapshots tree
	 * that contains a record pointing at those very roots. COW makes
	 * this consistent — the snapshots-tree path is freshly allocated
	 * and doesn't intersect the just-committed roots. */
	if (tmp_->snapshots_tree != NULL) {
		tessera_snapshot_record_t srec;
		memset(&srec, 0, sizeof srec);
		srec.generation         = tmp_->sb.generation;
		struct timeval _tv;
		getmicrotime(&_tv);
		srec.timestamp_ns       = (uint64_t)_tv.tv_sec * 1000000000ULL +
		                           (uint64_t)_tv.tv_usec * 1000ULL;
		srec.inode_root         = tmp_->sb.inode_root;
		srec.pack_registry_root = tmp_->sb.pack_registry_root;
		srec.free_extent_root   = tmp_->sb.free_extent_root;
		memcpy(srec.reason_tag, "auto", 4);

		uint8_t skey[8];
		for (int i = 0; i < 8; i++)
			skey[i] = (uint8_t)(srec.generation >> ((7 - i) * 8));
		uint64_t new_sroot = tmp_->sb.snapshots_root;
		if (tessera_btree_put(tmp_->snapshots_tree, skey, &srec,
		    &new_sroot) == TESSERA_OK) {
			tmp_->sb.snapshots_root = new_sroot;
			tmp_->sb.snapshots_gen++;
		}
		/* btree_put failure is non-fatal — losing one snapshot
		 * record doesn't break the live mount; future commits
		 * will retry. */
	}

	if (tmp_->journal != NULL) {
		uint64_t tx;
		if (tessera_journal_tx_begin(tmp_->journal, &tx,
		    "sb_commit") == TESSERA_OK) {
			struct tessera_jrec_sb_commit body;
			body.generation         = tmp_->sb.generation;
			body.inode_root         = tmp_->sb.inode_root;
			body.pack_registry_root = tmp_->sb.pack_registry_root;
			body.free_extent_root   = tmp_->sb.free_extent_root;
			(void)tessera_journal_append(tmp_->journal, tx,
			    TESSERA_ROOT_UPDATE, &body, sizeof body);
			(void)tessera_journal_tx_commit(tmp_->journal, tx);
		}
	}

	/* Crash-injection knob: simulate the journal-tx-committed-but-SB-
	 * write-failed window that replay-on-mount is supposed to cover.
	 * Auto-clears so the next commit goes through normally — i.e.
	 * the user gets exactly one "crashed" commit. We also skip the
	 * journal checkpoint below so the just-appended record stays
	 * around to be replayed on the next mount. */
	if (tessera_skip_next_sb) {
		tessera_skip_next_sb = 0;
		printf("tessera_fs: crash-injection — SB write skipped, "
		    "journal record retained for replay\n");
		return (0);
	}

	/* Heap-allocated — sector-sized array on the stack would risk
	 * kstack overflow when commit_sb is called from a deep frame
	 * (e.g., mount-time GC: mountfs → gc_data_zone → commit_sb). */
	uint8_t *buf = malloc(TESSERA_SECTOR_SIZE, M_TESSERA, M_WAITOK | M_ZERO);
	if (tessera_encode_superblock(&tmp_->sb, buf) != TESSERA_OK) {
		free(buf, M_TESSERA);
		return (EIO);
	}
	if (tessera_kbio_write(&tmp_->bio_ctx, 0, buf) != 0) {
		free(buf, M_TESSERA);
		return (EIO);
	}
	if (tessera_kbio_write(&tmp_->bio_ctx, 1, buf) != 0) {
		free(buf, M_TESSERA);
		return (EIO);
	}
	free(buf, M_TESSERA);

	/* SB durably advanced. The journal record we just appended is now
	 * applied; checkpoint frees its sectors so the next commit doesn't
	 * push us toward the journal-full wrap. Crash between the SB write
	 * and the checkpoint is harmless: replay-on-mount checks
	 * `record.gen > sb.gen` so the already-applied record is skipped. */
	if (tmp_->journal != NULL)
		(void)tessera_journal_checkpoint(tmp_->journal);

	/* The new SB is durable, so the OLD SB no longer references any
	 * of the meta-reserve sectors freed during this commit cycle. Move
	 * them from the pending list to the reusable free list. The next
	 * meta_alloc will pop from there before pushing the bump pointer.
	 *
	 * KNOWN ISSUE (v2 snapshots): retained-snapshot sector pinning is
	 * NOT enforced here. If a sector freed during this commit is still
	 * reachable from a retained snapshot's tree, this drain hands it
	 * back for reuse, eventually corrupting time-machine reads of
	 * older generations. The mount-time meta-mark recycler does walk
	 * snapshot trees, so freshly-mounted sessions stay consistent for
	 * one cycle — but mid-mount mutations break older snapshots. The
	 * proper fix needs a way to filter the drain by snapshot
	 * reachability without performing a full multi-tree walk per
	 * commit (too slow + stack-heavy in callout context). Slice 4
	 * (retention) will likely sidestep this by capping snapshot
	 * lifetime so the bug's impact is bounded. */
	if (tmp_->meta_pending_count > 0 && tmp_->meta_free != NULL) {
		uint32_t need = tmp_->meta_free_count + tmp_->meta_pending_count;
		if (need <= tmp_->meta_free_cap) {
			memcpy(tmp_->meta_free + tmp_->meta_free_count,
			    tmp_->meta_pending,
			    tmp_->meta_pending_count * sizeof(uint64_t));
			tmp_->meta_free_count = need;
		}
		/* If overflow (shouldn't happen — caps are sized to the
		 * reserve length), drop the pending list — those sectors
		 * just leak this session, same as the old bump-only path. */
		tmp_->meta_pending_count = 0;
	}
	return (0);
}

/* ── v2-step-2a deferred-commit helpers ───────────────────────────
 *
 * Concurrency note: these match the pre-existing kmod assumption that
 * mutations on a given mount are serialised by the surrounding VFS
 * lock structure (vnode locks + mount-level conventions). The flush
 * task runs in a kernel taskqueue thread; it can race with vops, but
 * the same race already exists between any two concurrent vops
 * touching tmp_->sb fields. Phase 2b adds explicit serialisation
 * when the in-memory dirty-inode set lands.
 */

static void
tessera_fs_flush_task(void *ctx, int pending)
{
	(void)pending;
	(void)tessera_fs_flush((struct tessera_mount *)ctx);
}

static void
tessera_fs_flush_callout(void *arg)
{
	struct tessera_mount *tmp_ = arg;
	if (tmp_->flush_unmounting) return;
	(void)taskqueue_enqueue(taskqueue_thread, &tmp_->flush_task);
}

static void
tessera_fs_mark_dirty(struct tessera_mount *tmp_)
{
	tessera_stat_mark_dirty++;
	tmp_->sb_dirty = 1;
	if (tmp_->flush_co_init && !tmp_->flush_unmounting) {
		int t = tessera_flush_interval_sec;
		if (t < 1) t = 1;
		callout_reset(&tmp_->flush_co, hz * t,
		    tessera_fs_flush_callout, tmp_);
	}

	/*
	 * Meta-reserve pressure trigger. Each per-op btree COW consumes
	 * meta-reserve sectors; meta_pending only drains to meta_free
	 * inside commit_sb's success path (round 7-step3). Deferring SB
	 * commits indefinitely would walk the bump pointer to ENOSPC in
	 * a few hundred touches. When usage crosses 75%, flush
	 * synchronously to drain pending. The flush itself COWs one
	 * more level so we leave headroom below 100% for that.
	 */
	const uint64_t used = tmp_->sb.meta_reserve_bump
	    - tmp_->sb.meta_reserve_start;
	const uint64_t cap  = tmp_->sb.meta_reserve_length;
	const uint64_t free = (uint64_t)tmp_->meta_free_count;
	if (cap > 0 && (used > free) &&
	    (used - free) * 4 >= cap * 3) {
		(void)tessera_fs_flush(tmp_);
	}
}

static int
tessera_fs_flush(struct tessera_mount *tmp_)
{
	if (!tmp_->sb_dirty) return (0);
	int r = tessera_commit_sb(tmp_);
	if (r == 0) tmp_->sb_dirty = 0;
	return (r);
}

/* Visitor used at mount time to mark which meta-reserve sectors are
 * still referenced by a live tree. ctx is `struct meta_mark_ctx`
 * (forward-declared above). */
static int
meta_mark_visitor(void *ctx, uint64_t sector)
{
	struct meta_mark_ctx *m = ctx;
	if (sector >= m->lo && sector < m->hi) {
		uint64_t bit = sector - m->lo;
		m->bitmap[bit / 8] |= (uint8_t)(1u << (bit % 8));
	}
	return (0);
}

/*
 * Journal replay handler. Called once per record during
 * tessera_journal_replay's walk; we apply the highest-generation
 * ROOT_UPDATE record to the in-memory SB. Caller must follow up
 * with a write of the SB sectors after the walk completes.
 *
 * The walk happens BEFORE inode_tree / pack_registry / extent_alloc
 * opens at mount time, so the trees end up opened against the
 * rolled-forward roots. This recovers from a crash that landed
 * between tx_commit and the actual SB sector writes.
 */
static int
tessera_replay_handler(void *ctx, const tessera_record_header_t *hdr,
                       const uint8_t *body)
{
	struct tessera_replay_ctx *rc = ctx;
	if (hdr->record_type != (uint32_t)TESSERA_ROOT_UPDATE) return (0);
	if (hdr->body_length < sizeof(struct tessera_jrec_sb_commit))
		return (0);
	struct tessera_jrec_sb_commit rec;
	memcpy(&rec, body, sizeof rec);
	if (rec.generation <= rc->tmp_->sb.generation) return (0);
	rc->tmp_->sb.generation         = rec.generation;
	rc->tmp_->sb.inode_root         = rec.inode_root;
	rc->tmp_->sb.pack_registry_root = rec.pack_registry_root;
	rc->tmp_->sb.free_extent_root   = rec.free_extent_root;
	rc->applied++;
	return (0);
}

/* Flush the in-memory free-extent allocator back to a fresh on-disk
 * tree (allocated from the metadata reserve, NOT recursively from
 * the data zone) and return the new root sector via tmp_->sb. */
static int
tessera_commit_extent(struct tessera_mount *tmp_)
{
	uint64_t new_root = 0;
	if (tessera_extent_flush_via(tmp_->extent_alloc, &tmp_->meta_bio,
	    &new_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.free_extent_root = new_root;
	tmp_->sb.free_extent_gen  = tmp_->sb.generation + 1;
	return (0);
}

/* ── in-kernel blob fetcher ──────────────────────────────────── */

/*
 * Fetch a blob from the pack zone by its SHA-256 hash.
 *
 * Strategy: walk the pack-registry B+tree; for each registered pack,
 * bread its sectors into a malloc'd buffer, hand that to the
 * userspace-style tessera_pack_reader, lookup by hash, copy the
 * matching bytes out before freeing the pack buffer.
 *
 * On success: *out_buf points at a freshly-allocated copy (caller
 * frees with M_TESSERA), *out_len is the byte length, return 0.
 * Returns ENOENT if no pack contains the blob.
 *
 * Round-4b note: this whole-pack-into-RAM strategy is wasteful for
 * large packs but correct and small. Round-5+ replaces it with a
 * sector-on-demand reader that walks the on-disk header → index →
 * data layout via bread, never materialising the whole pack.
 */
#define TESSERA_FETCH_PACK_MAX_SECTORS  4096u   /* 16 MiB cap */

static int
tessera_fs_fetch_blob(struct tessera_mount *tmp_,
                      const tessera_hash_t hash,
                      uint8_t **out_buf, uint32_t *out_len)
{
	if (tmp_->pack_registry_tree == NULL) return (ENOENT);
	tessera_btree_cursor_t *c =
	    tessera_btree_seek_first(tmp_->pack_registry_tree);
	if (c == NULL) return (ENOENT);

	int rc = ENOENT;
	for (;;) {
		uint8_t key[16];
		uint8_t value[TESSERA_REGISTRY_ENTRY_SIZE];
		if (tessera_btree_cursor_get(c, key, value) != 0) break;

		tessera_registry_entry_t re;
		if (tessera_decode_registry_entry(value, &re) != TESSERA_OK)
			goto next_pack;
		if (re.length_sectors == 0 ||
		    re.length_sectors > TESSERA_FETCH_PACK_MAX_SECTORS) {
			printf("tessera_fs: skipping pack with length=%lu sectors "
			    "(zero or > %u-sector cap)\n",
			    (unsigned long)re.length_sectors,
			    TESSERA_FETCH_PACK_MAX_SECTORS);
			goto next_pack;
		}

		const size_t pack_len =
		    (size_t)re.length_sectors * TESSERA_SECTOR_SIZE;
		uint8_t *packbuf = malloc(pack_len, M_TESSERA, M_NOWAIT);
		if (packbuf == NULL) goto next_pack;

		int read_ok = 1;
		for (uint64_t i = 0; i < re.length_sectors; i++) {
			struct buf *bp = NULL;
			int err = bread(tmp_->devvp,
			    (re.start_sector + i) *
			        btodb(TESSERA_SECTOR_SIZE),
			    TESSERA_SECTOR_SIZE,
			    tmp_->bio_ctx.cred ? tmp_->bio_ctx.cred : NOCRED,
			    &bp);
			if (err != 0) {
				if (bp != NULL) brelse(bp);
				read_ok = 0;
				break;
			}
			memcpy(packbuf + i * TESSERA_SECTOR_SIZE,
			    bp->b_data, TESSERA_SECTOR_SIZE);
			brelse(bp);
		}
		if (!read_ok) {
			free(packbuf, M_TESSERA);
			goto next_pack;
		}

		tessera_pack_reader_t *pr = tessera_pack_open(packbuf, pack_len);
		if (pr != NULL) {
			const uint8_t *bytes = NULL;
			uint32_t blen = 0;
			if (tessera_pack_lookup(pr, hash, &bytes, &blen)
			    == TESSERA_OK) {
				uint8_t *copy = malloc(blen, M_TESSERA, M_WAITOK);
				memcpy(copy, bytes, blen);
				*out_buf = copy;
				*out_len = blen;
				tessera_pack_close(pr);
				free(packbuf, M_TESSERA);
				rc = 0;
				break;
			}
			tessera_pack_close(pr);
		}
		free(packbuf, M_TESSERA);

next_pack:
		if (tessera_btree_cursor_next(c) != 0) break;
	}
	tessera_btree_cursor_free(c);
	return (rc);
}

/* ── Round 6d: blob publishing + vop_remove ──────────────────── */

/*
 * Mount-time data-zone GC.
 *
 * Walks the pack registry; for each pack that contains exactly one
 * blob (the kmod's publish_manifest always emits single-blob packs),
 * reads the pack's index sector to get that blob's hash, and checks
 * if any live inode's manifest_hash matches. Unreferenced single-blob
 * packs are deleted: their registry entry is removed and their data-
 * zone sectors are returned to the extent allocator.
 *
 * Conservative — never touches multi-blob packs (mkfs-seeded packs
 * carry multiple blobs and have lifetime semantics tied to their
 * directory manifests). A future GC pass could handle those by
 * traversing CHUNK_LIST manifests and intersecting blob-by-blob.
 *
 * Returns the number of packs reclaimed.
 */
static int
tessera_fs_gc_data_zone(struct tessera_mount *tmp_)
{
	printf("tessera_fs: gc enter\n");
	if (tmp_->inode_tree == NULL || tmp_->pack_registry_tree == NULL)
		return (0);

	/* Pass 1: collect every live manifest_hash. v2 unionises across
	 * the current SB's inode_tree AND every retained snapshot's
	 * inode_tree — without that, a manifest pack referenced only by
	 * an older snapshot would be reclaimed and the snapshot would
	 * silently corrupt. COW means many btree nodes are shared, so
	 * the cost in practice is much smaller than O(snapshots × tree). */
	uint32_t live_cap = 64, live_count = 0;
	tessera_hash_t *live = malloc(live_cap * sizeof(*live), M_TESSERA,
	    M_WAITOK);

#define _GC_PUSH_HASH(_h) do {                                            \
	if (tessera_hash_is_null(_h)) break;                              \
	if (live_count == live_cap) {                                     \
		live_cap *= 2;                                            \
		tessera_hash_t *grown = malloc(                           \
		    live_cap * sizeof(*grown), M_TESSERA, M_WAITOK);      \
		memcpy(grown, live, live_count * sizeof(*live));          \
		free(live, M_TESSERA);                                    \
		live = grown;                                             \
	}                                                                 \
	memcpy(live[live_count], _h, sizeof(tessera_hash_t));             \
	live_count++;                                                     \
} while (0)

	/* Helper: walk one opened inode_tree, append all manifest hashes. */
#define _GC_WALK_INODE_TREE(_tree) do {                                   \
	if ((_tree) == NULL) break;                                       \
	tessera_btree_cursor_t *_c = tessera_btree_seek_first(_tree);     \
	if (_c == NULL) break;                                            \
	for (;;) {                                                        \
		uint8_t _k[4];                                            \
		tessera_inode_record_t _ino;                              \
		if (tessera_btree_cursor_get(_c, _k, &_ino) != TESSERA_OK)\
			break;                                            \
		_GC_PUSH_HASH(_ino.manifest_hash);                        \
		if (tessera_btree_cursor_next(_c) != TESSERA_OK) break;   \
	}                                                                 \
	tessera_btree_cursor_free(_c);                                    \
} while (0)

	_GC_WALK_INODE_TREE(tmp_->inode_tree);
	printf("tessera_fs: gc pass1 — %u live hashes from current SB\n",
	    live_count);

	/* Snapshot inode_trees */
	if (tmp_->snapshots_tree != NULL) {
		tessera_btree_cursor_t *sc =
		    tessera_btree_seek_first(tmp_->snapshots_tree);
		uint32_t snap_count = 0;
		while (sc != NULL) {
			uint8_t sk[8];
			tessera_snapshot_record_t srec;
			if (tessera_btree_cursor_get(sc, sk, &srec)
			    != TESSERA_OK) break;
			/* Skip the just-current snapshot; its tree is the
			 * same root as tmp_->sb.inode_root and we already
			 * walked it. */
			if (srec.inode_root != 0 &&
			    srec.inode_root != tmp_->sb.inode_root) {
				tessera_btree_t *snap_inode =
				    tessera_btree_open(&tmp_->meta_bio,
				        srec.inode_root, /*tree_kind*/ 0,
				        /*key*/ 4,
				        /*value*/ TESSERA_INODE_RECORD_SIZE);
				_GC_WALK_INODE_TREE(snap_inode);
				if (snap_inode != NULL)
					tessera_btree_close(snap_inode);
				snap_count++;
			}
			if (tessera_btree_cursor_next(sc) != TESSERA_OK) break;
		}
		if (sc != NULL) tessera_btree_cursor_free(sc);
		if (snap_count > 0)
			printf("tessera_fs: gc pass1 — %u retained snapshots "
			    "unionised; %u total live hashes\n",
			    snap_count, live_count);
	}
#undef _GC_WALK_INODE_TREE
#undef _GC_PUSH_HASH

	/* Pass 2: walk pack_registry_tree, identify dead single-blob
	 * packs. We collect (pack_id, start_sector, length_sectors)
	 * tuples first then mutate the tree afterwards — mutating mid-
	 * walk would invalidate the cursor. */
	struct dead { uint8_t pack_id[16]; uint64_t start; uint64_t len; };
	uint32_t dead_cap = 16, dead_count = 0;
	struct dead *deads = malloc(dead_cap * sizeof(*deads), M_TESSERA,
	    M_WAITOK);

	tessera_btree_cursor_t *pc =
	    tessera_btree_seek_first(tmp_->pack_registry_tree);
	if (pc != NULL) {
		for (;;) {
			uint8_t pkey[16];
			uint8_t pval[TESSERA_REGISTRY_ENTRY_SIZE];
			if (tessera_btree_cursor_get(pc, pkey, pval) != TESSERA_OK)
				break;
			tessera_registry_entry_t re;
			if (tessera_decode_registry_entry(pval, &re)
			    != TESSERA_OK) goto next_p;
			if (re.blob_count != 1) goto next_p; /* skip multi-blob */
			if (re.length_sectors == 0 ||
			    re.length_sectors > 4096u) goto next_p; /* sanity */

			/* Read the pack's first sector — that's the header,
			 * which contains a pointer to the index. To get the
			 * blob hash we need the full pack (or at least the
			 * index sector). For simplicity, materialise the
			 * whole pack — same path as fetch_blob already uses. */
			const size_t pack_len = (size_t)re.length_sectors *
			    TESSERA_SECTOR_SIZE;
			uint8_t *packbuf = malloc(pack_len, M_TESSERA, M_WAITOK);
			int ok = 1;
			for (uint64_t i = 0; i < re.length_sectors; i++) {
				struct buf *bp = NULL;
				if (bread(tmp_->devvp,
				    (re.start_sector + i) *
				        btodb(TESSERA_SECTOR_SIZE),
				    TESSERA_SECTOR_SIZE,
				    tmp_->bio_ctx.cred ? tmp_->bio_ctx.cred
				    : NOCRED, &bp) != 0) {
					if (bp) brelse(bp);
					ok = 0; break;
				}
				memcpy(packbuf + i * TESSERA_SECTOR_SIZE,
				    bp->b_data, TESSERA_SECTOR_SIZE);
				brelse(bp);
			}
			if (!ok) { free(packbuf, M_TESSERA); goto next_p; }
			tessera_pack_reader_t *pr = tessera_pack_open(packbuf,
			    pack_len);
			tessera_hash_t bh;
			int dead = 0;
			if (pr != NULL &&
			    tessera_pack_blob_hash_at(pr, 0, bh) == TESSERA_OK) {
				dead = 1;
				for (uint32_t li = 0; li < live_count; li++) {
					if (memcmp(bh, live[li],
					    sizeof(tessera_hash_t)) == 0) {
						dead = 0; break;
					}
				}
			}
			if (pr) tessera_pack_close(pr);
			free(packbuf, M_TESSERA);

			if (dead) {
				if (dead_count == dead_cap) {
					dead_cap *= 2;
					struct dead *grown = malloc(
					    dead_cap * sizeof(*grown),
					    M_TESSERA, M_WAITOK);
					memcpy(grown, deads,
					    dead_count * sizeof(*deads));
					free(deads, M_TESSERA);
					deads = grown;
				}
				memcpy(deads[dead_count].pack_id, pkey, 16);
				deads[dead_count].start = re.start_sector;
				deads[dead_count].len   = re.length_sectors;
				dead_count++;
			}
next_p:
			if (tessera_btree_cursor_next(pc) != TESSERA_OK) break;
		}
		tessera_btree_cursor_free(pc);
	}
	free(live, M_TESSERA);
	printf("tessera_fs: gc pass2 done — %u dead packs\n", dead_count);

	/* Pass 3: apply. btree_delete each dead key, extent_free each
	 * range, then commit. */
	uint64_t new_pack_root = tmp_->sb.pack_registry_root;
	for (uint32_t i = 0; i < dead_count; i++) {
		if (tessera_btree_delete(tmp_->pack_registry_tree,
		    deads[i].pack_id, &new_pack_root) == TESSERA_OK) {
			tmp_->sb.pack_registry_root = new_pack_root;
		}
		(void)tessera_extent_free(tmp_->extent_alloc,
		    deads[i].start, deads[i].len);
	}
	free(deads, M_TESSERA);

	printf("tessera_fs: gc pass3 done — committing\n");
	if (dead_count > 0) {
		(void)tessera_commit_extent(tmp_);
		printf("tessera_fs: gc post commit_extent\n");
		(void)tessera_commit_sb(tmp_);
		printf("tessera_fs: gc post commit_sb\n");
	}
	printf("tessera_fs: gc done\n");
	return ((int)dead_count);
}

/*
 * Publish a single-blob pack containing one finalized manifest.
 *
 *   1. sha256 the manifest bytes → pack_id (first 16 B) + blob hash.
 *   2. tessera_pack_begin/add_blob/finalize → encoded pack bytes.
 *   3. tessera_extent_alloc on the data zone → start sector.
 *   4. Synchronous bwrite each sector (data must reach disk before
 *      the registry entry pointing at it; bdwrite would let a crash
 *      between the registry put and the sb commit leave a dangling
 *      pointer at unwritten sectors).
 *   5. btree_put a registry entry through the pack_registry_tree.
 *
 * Caller is responsible for: updating any inode-tree records that
 * reference the new manifest_hash, calling tessera_commit_extent
 * (since we allocated data-zone sectors), and tessera_commit_sb.
 */
static int
tessera_fs_publish_manifest(struct tessera_mount *tmp_,
                            const uint8_t *manifest_bytes, size_t mlen,
                            tessera_hash_t out_hash)
{
	if (tmp_->pack_registry_tree == NULL || tmp_->extent_alloc == NULL)
		return (EROFS);

	tessera_sha256(manifest_bytes, mlen, out_hash);

	uint8_t pack_id[16];
	memcpy(pack_id, out_hash, 16);

	/* Publish-cache shortcut (v2 polish): pack_id is derived from the
	 * manifest hash, so identical content lands at the same pack_id.
	 * If the registry already contains an entry, the pack body is on
	 * disk and no work is needed. Catches cp(1)'s repeated whole-file
	 * republish and chmod-then-revert dirent churn for free. */
	{
		uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
		if (tessera_btree_get(tmp_->pack_registry_tree,
		    pack_id, reg_value) == TESSERA_OK) {
			tessera_stat_publish_dedup_manifest++;
			return (0);
		}
	}

	tessera_pack_builder_t *pb = tessera_pack_begin(0, pack_id, 0);
	if (pb == NULL) return (ENOMEM);
	if (tessera_pack_add_blob(pb, out_hash, manifest_bytes,
	    (uint32_t)mlen, TESSERA_BLOB_FLAG_MANIFEST) != TESSERA_OK) {
		tessera_pack_free(pb);
		return (EIO);
	}

	size_t pack_size = 0;
	(void)tessera_pack_finalize(pb, NULL, 0, &pack_size);
	if (pack_size == 0 || (pack_size % TESSERA_SECTOR_SIZE) != 0) {
		tessera_pack_free(pb);
		return (EIO);
	}
	uint8_t *pack_bytes = malloc(pack_size, M_TESSERA, M_WAITOK);
	int r = tessera_pack_finalize(pb, pack_bytes, pack_size, &pack_size);
	tessera_pack_free(pb);
	if (r != TESSERA_OK) { free(pack_bytes, M_TESSERA); return (EIO); }

	const uint64_t n_sectors = pack_size / TESSERA_SECTOR_SIZE;
	uint64_t pack_start = 0;
	if (tessera_extent_alloc(tmp_->extent_alloc, n_sectors,
	    &pack_start) != TESSERA_OK) {
		free(pack_bytes, M_TESSERA);
		return (ENOSPC);
	}
	for (uint64_t i = 0; i < n_sectors; i++) {
		if (tessera_kbio_write(&tmp_->bio_ctx, pack_start + i,
		    pack_bytes + i * TESSERA_SECTOR_SIZE) != 0) {
			free(pack_bytes, M_TESSERA);
			return (EIO);
		}
	}
	free(pack_bytes, M_TESSERA);

	tessera_registry_entry_t re;
	memset(&re, 0, sizeof re);
	memcpy(re.pack_id, pack_id, 16);
	re.start_sector    = pack_start;
	re.length_sectors  = n_sectors;
	re.blob_count      = 1;
	re.pack_kind       = 0;
	re.total_bytes     = pack_size;
	re.create_time     = 0;
	re.reachable_blobs = 1;
	re.flags           = TESSERA_REGISTRY_FLAG_SEALED;
	uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
	if (tessera_encode_registry_entry(&re, reg_value) != TESSERA_OK)
		return (EIO);

	uint64_t new_pack_root = tmp_->sb.pack_registry_root;
	if (tessera_btree_put(tmp_->pack_registry_tree, pack_id, reg_value,
	    &new_pack_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.pack_registry_root = new_pack_root;
	return (0);
}

/* v2-step-3a: bundle N chunks + the manifest into ONE pack. Without
 * this, each chunk would publish via single-blob pack like
 * publish_manifest does — ~4 KiB header + 4 KiB footer per chunk =
 * ~32% storage overhead for 64 KiB chunks. Multi-blob packing drops
 * the overhead to ~0.1%. The chunks come first in the pack body, the
 * manifest last; pack_id derives from the manifest hash so re-emitting
 * the same content lands at the same pack_id. */
static int
tessera_fs_publish_chunked(struct tessera_mount *tmp_,
    const struct tessera_chunk_in *chunks, uint32_t n_chunks,
    const uint8_t *manifest_bytes, size_t mlen,
    tessera_hash_t out_manifest_hash)
{
	if (tmp_->pack_registry_tree == NULL || tmp_->extent_alloc == NULL)
		return (EROFS);

	tessera_sha256(manifest_bytes, mlen, out_manifest_hash);

	uint8_t pack_id[16];
	memcpy(pack_id, out_manifest_hash, 16);

	/* Publish-cache shortcut (v2 polish). See publish_manifest. The
	 * chunked variant gets even more value because cp(1)-driven
	 * repeated whole-file rewrites land at the same pack_id once
	 * each chunk's content stops changing. */
	{
		uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
		if (tessera_btree_get(tmp_->pack_registry_tree,
		    pack_id, reg_value) == TESSERA_OK) {
			tessera_stat_publish_dedup_chunked++;
			return (0);
		}
	}

	tessera_pack_builder_t *pb = tessera_pack_begin(2 /* mixed */,
	    pack_id, 0);
	if (pb == NULL) return (ENOMEM);

	for (uint32_t i = 0; i < n_chunks; i++) {
		if (tessera_pack_add_blob(pb, chunks[i].hash,
		    chunks[i].bytes, chunks[i].len,
		    TESSERA_BLOB_FLAG_CHUNK) != TESSERA_OK) {
			tessera_pack_free(pb);
			return (EIO);
		}
	}
	if (tessera_pack_add_blob(pb, out_manifest_hash, manifest_bytes,
	    (uint32_t)mlen, TESSERA_BLOB_FLAG_MANIFEST) != TESSERA_OK) {
		tessera_pack_free(pb);
		return (EIO);
	}

	size_t pack_size = 0;
	(void)tessera_pack_finalize(pb, NULL, 0, &pack_size);
	if (pack_size == 0 || (pack_size % TESSERA_SECTOR_SIZE) != 0) {
		tessera_pack_free(pb);
		return (EIO);
	}
	uint8_t *pack_bytes = malloc(pack_size, M_TESSERA, M_WAITOK);
	int r = tessera_pack_finalize(pb, pack_bytes, pack_size, &pack_size);
	tessera_pack_free(pb);
	if (r != TESSERA_OK) { free(pack_bytes, M_TESSERA); return (EIO); }

	const uint64_t n_sectors = pack_size / TESSERA_SECTOR_SIZE;
	uint64_t pack_start = 0;
	if (tessera_extent_alloc(tmp_->extent_alloc, n_sectors,
	    &pack_start) != TESSERA_OK) {
		free(pack_bytes, M_TESSERA);
		return (ENOSPC);
	}
	for (uint64_t i = 0; i < n_sectors; i++) {
		if (tessera_kbio_write(&tmp_->bio_ctx, pack_start + i,
		    pack_bytes + i * TESSERA_SECTOR_SIZE) != 0) {
			free(pack_bytes, M_TESSERA);
			return (EIO);
		}
	}
	free(pack_bytes, M_TESSERA);

	tessera_registry_entry_t re;
	memset(&re, 0, sizeof re);
	memcpy(re.pack_id, pack_id, 16);
	re.start_sector    = pack_start;
	re.length_sectors  = n_sectors;
	re.blob_count      = n_chunks + 1u;
	re.pack_kind       = 2;
	re.total_bytes     = pack_size;
	re.create_time     = 0;
	re.reachable_blobs = n_chunks + 1u;
	re.flags           = TESSERA_REGISTRY_FLAG_SEALED;
	uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
	if (tessera_encode_registry_entry(&re, reg_value) != TESSERA_OK)
		return (EIO);

	uint64_t new_pack_root = tmp_->sb.pack_registry_root;
	if (tessera_btree_put(tmp_->pack_registry_tree, pack_id, reg_value,
	    &new_pack_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.pack_registry_root = new_pack_root;
	return (0);
}

/* v2-step-3a: chunked replacement. Splits `new_bytes` into fixed-size
 * chunks (TESSERA_CHUNK_SIZE), hashes each, builds a CHUNK_LIST
 * manifest pointing at them, and publishes everything in one pack.
 * The inode record is then COW'd to point at the new manifest hash.
 *
 * v3 design will swap fixed-size for adaptive (size scales with file
 * size to keep manifest bounded) and add range-aware modification
 * (only rewrite affected chunks, retain hashes for the rest). For
 * step-3a we stay correct + simple: rewrite every chunk. The
 * substrate-level dedup in publish_chunked still wins back the cost
 * for repeated identical content. */
#define TESSERA_INLINE_THRESHOLD  (256u * 1024u)

/*
 * Adaptive chunk size by file size (step-3b).
 *
 *     final_size < 64 MiB  → 64 KiB chunks (16 records / MiB)
 *     final_size < 4 GiB   → 1 MiB chunks  (~1k records / GiB)
 *     final_size ≥ 4 GiB   → 4 MiB chunks  (~256 records / GiB)
 *
 * Without this, a 100 GiB file at 64 KiB chunks would produce ~1.6M
 * chunk_records (~75 MiB CHUNK_LIST manifest body) — past the point
 * where the linear-scan manifest design is reasonable. With adaptive
 * sizing, manifest stays under a few MiB even at 100 GiB.
 *
 * Trade-off: cross-tier writes (file grows from < 64 MiB to > 64 MiB)
 * can't dedup against prior chunks (different sizes → different
 * hashes). New chunks are re-published. The win on manifest size is
 * worth the dedup loss at tier transitions.
 *
 * CHUNK_TREE (step-3c) provides log(N)-amplification for VM-scale
 * files where small chunks are required.
 */
static inline uint32_t
tessera_chunk_size_for(uint64_t file_size)
{
	if (file_size <  (64ULL * 1024ULL * 1024ULL))         /* < 64 MiB */
		return ( 64u * 1024u);
	if (file_size <  ( 4ULL * 1024ULL * 1024ULL * 1024ULL))/* < 4 GiB */
		return (  1u * 1024u * 1024u);
	return (  4u * 1024u * 1024u);                        /* ≥ 4 GiB */
}

static int
tessera_fs_replace_content_chunked(struct tessera_mount *tmp_,
    uint32_t inode_no, const uint8_t *new_bytes, size_t new_len)
{
	if (new_len == 0) {
		/* Empty file = empty INLINE manifest, no chunks. */
		return tessera_fs_replace_content(tmp_, inode_no, new_bytes, 0);
	}

	/* Step-3b: chunk-level dedup vs the old manifest. Read the old
	 * inode + its CHUNK_LIST manifest (if any). Per slot, hash the
	 * new bytes; if the slot's old hash already matches, the chunk
	 * blob is already on disk in some prior pack — skip republishing.
	 * Storage cost of an unchanged-content rewrite drops from N
	 * chunks to ~zero (just the manifest pack itself). */

	uint8_t key[4];
	tessera_inode_record_t old_ino;
	encode_inode_key(inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &old_ino) != TESSERA_OK)
		return (EIO);

	const uint32_t cs = tessera_chunk_size_for(new_len);

	/* Slot-by-slot dedup only works when the prior write used the
	 * same chunk size we're about to use. If the file crossed a
	 * tier (e.g. < 64 MiB → > 64 MiB), the old slots are at the
	 * smaller size and won't match offsets/sizes; we just skip
	 * dedup in that case and let the new chunks publish fresh. */
	tessera_hash_t *old_hashes = NULL;
	uint32_t old_n = 0;
	{
		int has_old = 0;
		for (size_t i = 0; i < TESSERA_HASH_SIZE; i++)
			if (old_ino.manifest_hash[i] != 0) { has_old = 1; break; }
		if (has_old) {
			uint8_t *old_mft = NULL;
			uint32_t old_mlen = 0;
			if (tessera_fs_fetch_blob(tmp_, old_ino.manifest_hash,
			    &old_mft, &old_mlen) == 0) {
				tessera_manifest_parser_t *p =
				    tessera_manifest_parse(old_mft, old_mlen);
				if (p != NULL &&
				    tessera_manifest_parser_kind(p)
				        == TESSERA_MFT_CHUNK_LIST) {
					old_n = tessera_manifest_parser_count(p);
					old_hashes = malloc(
					    old_n * sizeof *old_hashes,
					    M_TESSERA, M_WAITOK | M_ZERO);
					for (uint32_t i = 0; i < old_n; i++) {
						tessera_chunk_record_t cr;
						if (tessera_manifest_chunk_at(p, i, &cr)
						    != TESSERA_OK) continue;
						if (cr.logical_offset ==
						    (uint64_t)i * cs)
							memcpy(old_hashes[i],
							    cr.chunk_hash,
							    sizeof cr.chunk_hash);
					}
				}
				if (p != NULL) tessera_manifest_parser_free(p);
				free(old_mft, M_TESSERA);
			}
		}
	}
	const uint32_t n_chunks = (uint32_t)((new_len + cs - 1) / cs);

	struct tessera_chunk_in *dirty = malloc(
	    n_chunks * sizeof(*dirty), M_TESSERA, M_WAITOK | M_ZERO);
	uint32_t n_dirty = 0;

	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_CHUNK_LIST);
	if (mb == NULL) {
		free(dirty, M_TESSERA);
		if (old_hashes) free(old_hashes, M_TESSERA);
		return (ENOMEM);
	}

	for (uint32_t i = 0; i < n_chunks; i++) {
		const uint64_t off = (uint64_t)i * cs;
		const uint32_t len = (off + cs <= new_len) ? cs
		    : (uint32_t)(new_len - off);

		/* Sparse-file detection (step-3b): if the chunk is all
		 * zeros, emit a ZERO_HOLE chunk_record with a zeroed hash
		 * — no blob published, no chunk bytes on disk. The read
		 * path returns zeros on seeing the flag. Same trick most
		 * extent FSes use; for VM disk images (sparse zeros
		 * dominate) this is the difference between 1 GiB and
		 * a few MiB on disk. */
		int all_zero = 1;
		for (uint32_t j = 0; j < len; j++) {
			if (new_bytes[off + j] != 0) { all_zero = 0; break; }
		}
		if (all_zero) {
			tessera_hash_t zh;
			memset(zh, 0, sizeof zh);
			if (tessera_manifest_add_chunk(mb, zh, off, len,
			    TESSERA_CHUNK_FLAG_ZERO_HOLE) != TESSERA_OK) {
				tessera_manifest_free(mb);
				free(dirty, M_TESSERA);
				if (old_hashes) free(old_hashes, M_TESSERA);
				return (ENOMEM);
			}
			tessera_stat_chunk_zero_hole++;
			continue;
		}

		tessera_hash_t h;
		tessera_sha256(new_bytes + off, len, h);

		if (tessera_manifest_add_chunk(mb, h, off, len, 0)
		    != TESSERA_OK) {
			tessera_manifest_free(mb);
			free(dirty, M_TESSERA);
			if (old_hashes) free(old_hashes, M_TESSERA);
			return (ENOMEM);
		}

		if (i < old_n &&
		    memcmp(h, old_hashes[i], TESSERA_HASH_SIZE) == 0) {
			/* Same content as before — already on disk. */
			tessera_stat_chunk_dedup_skip++;
			continue;
		}
		dirty[n_dirty].bytes = new_bytes + off;
		dirty[n_dirty].len   = len;
		memcpy(dirty[n_dirty].hash, h, sizeof h);
		n_dirty++;
	}
	if (old_hashes) free(old_hashes, M_TESSERA);

	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	uint8_t *mft = malloc(mlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, mft, mlen, &mlen, mhash)
	    != TESSERA_OK) {
		tessera_manifest_free(mb);
		free(mft, M_TESSERA);
		free(dirty, M_TESSERA);
		return (EIO);
	}
	tessera_manifest_free(mb);

	tessera_hash_t pub_hash;
	if (tessera_fs_publish_chunked(tmp_, dirty, n_dirty, mft, mlen,
	    pub_hash) != 0) {
		free(mft, M_TESSERA);
		free(dirty, M_TESSERA);
		return (EIO);
	}
	free(mft, M_TESSERA);
	free(dirty, M_TESSERA);

	tessera_inode_record_t ino;
	if (tessera_btree_get(tmp_->inode_tree, key, &ino) != TESSERA_OK)
		return (EIO);
	memcpy(ino.manifest_hash, pub_hash, sizeof pub_hash);
	ino.size = new_len;
	ino.gen++;
	struct timeval tv;
	getmicrotime(&tv);
	uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
	    (uint64_t)tv.tv_usec * 1000ULL;
	ino.mtime_ns = ino.ctime_ns = now_ns;

	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_put(tmp_->inode_tree, key, &ino,
	    &new_inode_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.inode_root = new_inode_root;
	return (0);
}

/*
 * Append fast-path (step-3b).
 *
 * For chunked CHUNK_LIST files where the write begins at exactly the
 * old file size, do the work in O(append_len) instead of O(file_size):
 *
 *   - Old chunks 0..N-2 keep their hashes verbatim — never fetched,
 *     never rehashed, just referenced from the new manifest.
 *   - If the old last chunk is partial, fetch ONLY that chunk
 *     (≤ cs bytes), merge with the head of the append, hash + queue.
 *     Aligned EOF (old_size % cs == 0) skips even that.
 *   - Remaining append bytes form additional chunks straight from
 *     the caller's buffer. ZERO_HOLE detection per chunk.
 *
 * Returns ENOTSUP if the file isn't eligible (size==0, INLINE
 * manifest, mismatched chunk size, malformed records). Caller is
 * expected to fall back to the slow path.
 *
 * 1 KiB log line appended to a 1 GiB file: ~0.005% of the bytes
 * touched compared with the slow whole-file rewrite.
 */
static int
tessera_fs_append_chunked(struct tessera_mount *tmp_, uint32_t inode_no,
    const uint8_t *append_bytes, size_t append_len, uint32_t cs)
{
	if (append_len == 0) return (ENOTSUP);

	uint8_t key[4];
	tessera_inode_record_t old_ino;
	encode_inode_key(inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &old_ino) != TESSERA_OK)
		return (EIO);

	const uint64_t old_size = old_ino.size;
	if (old_size == 0) return (ENOTSUP);   /* nothing to retain */
	const uint64_t new_size = old_size + (uint64_t)append_len;

	int has_old = 0;
	for (size_t i = 0; i < TESSERA_HASH_SIZE; i++)
		if (old_ino.manifest_hash[i] != 0) { has_old = 1; break; }
	if (!has_old) return (ENOTSUP);

	uint8_t  *old_mft = NULL;
	uint32_t  old_mlen = 0;
	if (tessera_fs_fetch_blob(tmp_, old_ino.manifest_hash,
	    &old_mft, &old_mlen) != 0) return (ENOTSUP);

	tessera_manifest_parser_t *p = tessera_manifest_parse(old_mft, old_mlen);
	if (p == NULL) {
		free(old_mft, M_TESSERA);
		return (ENOTSUP);
	}
	if (tessera_manifest_parser_kind(p) != TESSERA_MFT_CHUNK_LIST) {
		tessera_manifest_parser_free(p);
		free(old_mft, M_TESSERA);
		return (ENOTSUP);
	}

	const uint32_t old_n = tessera_manifest_parser_count(p);
	if (old_n == 0) {
		tessera_manifest_parser_free(p);
		free(old_mft, M_TESSERA);
		return (ENOTSUP);
	}

	/* Snapshot old records — we need them after parser is freed. */
	struct old_rec {
		tessera_hash_t hash;
		uint64_t       off;
		uint32_t       sz;
		uint32_t       flags;
	};
	struct old_rec *old = malloc(old_n * sizeof *old, M_TESSERA,
	    M_WAITOK | M_ZERO);
	int eligible = 1;
	for (uint32_t i = 0; i < old_n; i++) {
		tessera_chunk_record_t cr;
		if (tessera_manifest_chunk_at(p, i, &cr) != TESSERA_OK ||
		    cr.logical_offset != (uint64_t)i * cs ||
		    (i < old_n - 1 && cr.uncompressed_size != cs)) {
			eligible = 0;
			break;
		}
		memcpy(old[i].hash, cr.chunk_hash, sizeof cr.chunk_hash);
		old[i].off   = cr.logical_offset;
		old[i].sz    = cr.uncompressed_size;
		old[i].flags = cr.flags;
	}
	tessera_manifest_parser_free(p);
	free(old_mft, M_TESSERA);
	if (!eligible) {
		free(old, M_TESSERA);
		return (ENOTSUP);
	}

	const uint32_t last_old_sz = old[old_n - 1].sz;
	const int last_partial = (last_old_sz < cs);

	/* If the old last chunk is partial, materialize it so we can
	 * merge with the head of the append. ZERO_HOLE → all zeros,
	 * no fetch needed. */
	uint8_t *merge_buf = NULL;   /* alive across publish_chunked */
	if (last_partial) {
		merge_buf = malloc(cs, M_TESSERA, M_WAITOK | M_ZERO);
		if (!(old[old_n - 1].flags & TESSERA_CHUNK_FLAG_ZERO_HOLE)) {
			uint8_t *cb = NULL;
			uint32_t cb_len = 0;
			if (tessera_fs_fetch_blob(tmp_, old[old_n - 1].hash,
			    &cb, &cb_len) != 0) {
				free(merge_buf, M_TESSERA);
				free(old, M_TESSERA);
				return (ENOTSUP);
			}
			memcpy(merge_buf, cb,
			    (last_old_sz < cb_len) ? last_old_sz : cb_len);
			free(cb, M_TESSERA);
		}
	}

	/* All eligibility checks passed. Build the new manifest. */
	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_CHUNK_LIST);
	if (mb == NULL) {
		if (merge_buf) free(merge_buf, M_TESSERA);
		free(old, M_TESSERA);
		return (ENOMEM);
	}

	/* Worst-case dirty-chunks: 1 (merged) + ceil(append_len / cs). */
	const uint32_t dirty_cap =
	    1u + (uint32_t)((append_len + cs - 1) / cs);
	struct tessera_chunk_in *dirty = malloc(
	    dirty_cap * sizeof *dirty, M_TESSERA, M_WAITOK | M_ZERO);
	uint32_t n_dirty = 0;

	const uint32_t n_retained = last_partial ? (old_n - 1) : old_n;
	for (uint32_t i = 0; i < n_retained; i++) {
		if (tessera_manifest_add_chunk(mb, old[i].hash, old[i].off,
		    old[i].sz, old[i].flags) != TESSERA_OK)
			goto enomem;
	}

	size_t consumed = 0;
	uint64_t cur_off;

	if (last_partial) {
		const uint32_t fill = cs - last_old_sz;
		const uint32_t take = (append_len < fill) ? (uint32_t)append_len : fill;
		memcpy(merge_buf + last_old_sz, append_bytes, take);
		const uint32_t merged_sz = last_old_sz + take;
		const uint64_t merged_off = old[old_n - 1].off;

		int allzero = 1;
		for (uint32_t j = 0; j < merged_sz; j++)
			if (merge_buf[j] != 0) { allzero = 0; break; }

		if (allzero) {
			tessera_hash_t zh;
			memset(zh, 0, sizeof zh);
			if (tessera_manifest_add_chunk(mb, zh, merged_off,
			    merged_sz, TESSERA_CHUNK_FLAG_ZERO_HOLE)
			    != TESSERA_OK) goto enomem;
			tessera_stat_chunk_zero_hole++;
			free(merge_buf, M_TESSERA);
			merge_buf = NULL;
		} else {
			tessera_hash_t h;
			tessera_sha256(merge_buf, merged_sz, h);
			if (tessera_manifest_add_chunk(mb, h, merged_off,
			    merged_sz, 0) != TESSERA_OK) goto enomem;
			dirty[n_dirty].bytes = merge_buf;
			dirty[n_dirty].len   = merged_sz;
			memcpy(dirty[n_dirty].hash, h, sizeof h);
			n_dirty++;
		}
		consumed = take;
		cur_off  = merged_off + merged_sz;
	} else {
		cur_off = old_size;
	}

	while (consumed < append_len) {
		const size_t remaining = append_len - consumed;
		const uint32_t this_len = (remaining > cs) ? cs
		    : (uint32_t)remaining;
		const uint8_t *cb = append_bytes + consumed;

		int allzero = 1;
		for (uint32_t j = 0; j < this_len; j++)
			if (cb[j] != 0) { allzero = 0; break; }

		if (allzero) {
			tessera_hash_t zh;
			memset(zh, 0, sizeof zh);
			if (tessera_manifest_add_chunk(mb, zh, cur_off,
			    this_len, TESSERA_CHUNK_FLAG_ZERO_HOLE)
			    != TESSERA_OK) goto enomem;
			tessera_stat_chunk_zero_hole++;
		} else {
			tessera_hash_t h;
			tessera_sha256(cb, this_len, h);
			if (tessera_manifest_add_chunk(mb, h, cur_off,
			    this_len, 0) != TESSERA_OK) goto enomem;
			dirty[n_dirty].bytes = cb;
			dirty[n_dirty].len   = this_len;
			memcpy(dirty[n_dirty].hash, h, sizeof h);
			n_dirty++;
		}
		consumed += this_len;
		cur_off  += this_len;
	}

	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	uint8_t *mft = malloc(mlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, mft, mlen, &mlen, mhash)
	    != TESSERA_OK) {
		free(mft, M_TESSERA);
		goto enomem;
	}
	tessera_manifest_free(mb);

	tessera_hash_t pub_hash;
	int rc = tessera_fs_publish_chunked(tmp_, dirty, n_dirty, mft, mlen,
	    pub_hash);
	free(mft, M_TESSERA);
	if (merge_buf) free(merge_buf, M_TESSERA);
	free(dirty, M_TESSERA);
	free(old, M_TESSERA);
	if (rc != 0) return (rc);

	tessera_inode_record_t ino;
	if (tessera_btree_get(tmp_->inode_tree, key, &ino) != TESSERA_OK)
		return (EIO);
	memcpy(ino.manifest_hash, pub_hash, sizeof pub_hash);
	ino.size = new_size;
	ino.gen++;
	struct timeval tv;
	getmicrotime(&tv);
	uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
	    (uint64_t)tv.tv_usec * 1000ULL;
	ino.mtime_ns = ino.ctime_ns = now_ns;

	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_put(tmp_->inode_tree, key, &ino,
	    &new_inode_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.inode_root = new_inode_root;
	return (0);

enomem:
	tessera_manifest_free(mb);
	if (merge_buf) free(merge_buf, M_TESSERA);
	free(dirty, M_TESSERA);
	free(old, M_TESSERA);
	return (ENOMEM);
}

/*
 * vop_remove — unlink a regular file from a directory.
 *
 * Plan:
 *   1. Fetch parent's DIRECTORY manifest blob.
 *   2. Find dirent matching cnp; pull child_inode_no.
 *   3. Build new DIRECTORY manifest body without that entry; finalize
 *      → new manifest bytes + hash via the manifest builder.
 *   4. Publish the new directory manifest as a blob.
 *   5. Update parent's inode record (manifest_hash = new hash) via
 *      btree_put on inode_tree.
 *   6. Delete child's inode record from inode_tree (via btree_delete).
 *   7. commit_extent + commit_sb so the change is persistent.
 *
 * Doesn't currently free the orphaned child manifest / chunk blobs in
 * the data zone — those leak until offline GC reclaims them. v1
 * accepts that (per tessera-fs §3.3 design notes).
 */
static int
tessera_vop_remove(struct vop_remove_args *ap)
{
	struct vnode *dvp = ap->a_dvp;
	struct vnode *vp  = ap->a_vp;
	struct componentname *cnp = ap->a_cnp;
	struct tessera_mount *tmp_ = VFSTOTESSERA(dvp->v_mount);
	struct tessera_node  *dn = VTOTNODE(dvp);
	struct tessera_node  *cn = VTOTNODE(vp);

	if (tmp_->inode_tree == NULL) return (EROFS);
	/* The kmod leaves vp->v_type == VNON in lookup (we never run a
	 * synthetic VOP_GETATTR there); VFS only routes VOP_REMOVE for
	 * non-directory targets, so trust the caller. */

	int err = 0;
	uint8_t  *blob = NULL;
	uint32_t  blob_len = 0;
	uint8_t  *new_mft = NULL;

	/* 1. Fetch parent's current DIRECTORY manifest. */
	uint8_t pkey[4];
	tessera_inode_record_t pino;
	encode_inode_key((uint32_t)dn->inode_no, pkey);
	if (tessera_btree_get(tmp_->inode_tree, pkey, &pino) != TESSERA_OK)
		return (EIO);
	if (tessera_fs_fetch_blob(tmp_, pino.manifest_hash,
	    &blob, &blob_len) != 0)
		return (EIO);
	if (blob_len < 32) { err = EIO; goto out; }
	const uint8_t *body = blob + 32;
	const size_t   blen = blob_len - 32;

	/* 2/3. Walk dirents; build new DIRECTORY manifest with all entries
	 *      EXCEPT the one whose name matches cnp. */
	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
	if (mb == NULL) { err = ENOMEM; goto out; }
	int found = 0;
	for (size_t off = 0; off + 10 <= blen;) {
		uint64_t entry_inode;
		uint16_t entry_nlen;
		memcpy(&entry_inode, body + off,     8);
		memcpy(&entry_nlen,  body + off + 8, 2);
		if (off + 10 + entry_nlen > blen) {
			tessera_manifest_free(mb);
			err = EIO; goto out;
		}
		const char *ename = (const char *)(body + off + 10);
		int match = (entry_nlen == cnp->cn_namelen) &&
		    (memcmp(ename, cnp->cn_nameptr, entry_nlen) == 0);
		if (match) {
			found = 1;
			if (entry_inode != cn->inode_no) {
				/* Stale vnode: dirent says some other inode. Bail
				 * rather than risk inconsistency. */
				tessera_manifest_free(mb);
				err = EIO; goto out;
			}
		} else {
			if (tessera_manifest_add_dirent(mb, entry_inode,
			    ename, entry_nlen) != TESSERA_OK) {
				tessera_manifest_free(mb);
				err = ENOMEM; goto out;
			}
		}
		off += 10 + entry_nlen;
	}
	if (!found) {
		tessera_manifest_free(mb);
		err = ENOENT; goto out;
	}

	size_t new_mlen = 0;
	tessera_hash_t new_mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &new_mlen, new_mhash);
	new_mft = malloc(new_mlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, new_mft, new_mlen, &new_mlen,
	    new_mhash) != TESSERA_OK) {
		tessera_manifest_free(mb);
		err = EIO; goto out;
	}
	tessera_manifest_free(mb);

	/* 4. Publish the new directory manifest as a single-blob pack. */
	tessera_hash_t pub_hash;
	if (tessera_fs_publish_manifest(tmp_, new_mft, new_mlen,
	    pub_hash) != 0) { err = EIO; goto out; }
	/* sanity: pub_hash must equal the manifest's own hash. */

	/* 5. Update parent inode record's manifest_hash. */
	memcpy(pino.manifest_hash, pub_hash, sizeof pub_hash);
	pino.gen++;
	pino.mtime_ns = pino.ctime_ns = pino.atime_ns;  /* leave timestamps; userspace can touch */
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_put(tmp_->inode_tree, pkey, &pino,
	    &new_inode_root) != TESSERA_OK) { err = EIO; goto out; }
	tmp_->sb.inode_root = new_inode_root;

	/* 6. Drop a link on the child. tessera_fs_inode_unlink decrements
	 * nlink; only btree_deletes the record when it hits 0, so
	 * hardlinks survive. */
	(void)tessera_fs_inode_unlink(tmp_, (uint32_t)cn->inode_no);

	/* 7. commit_extent + commit_sb so it persists across remount. */
	/* v2-step-2a: SB write is deferred; mark_dirty arms the flush
	 * callout. commit_extent stays synchronous because it advances
	 * tmp_->sb.free_extent_root in lockstep with on-disk btree node
	 * writes that cannot be replayed from the journal. */
	if (tessera_commit_extent(tmp_) != 0) {
		err = EIO; goto out;
	}
	tessera_fs_mark_dirty(tmp_);

out:
	if (blob)    free(blob,    M_TESSERA);
	if (new_mft) free(new_mft, M_TESSERA);
	return (err);
}

/*
 * Read the full current content of an inode (across INLINE / CHUNK_LIST
 * manifests) into a malloc'd buffer of `ino->size` bytes. Returns 0 on
 * success; on success *out_buf is M_TESSERA-allocated.
 */
static int
tessera_fs_read_full_content(struct tessera_mount *tmp_,
                             const tessera_inode_record_t *ino,
                             uint8_t **out_buf, size_t *out_size)
{
	*out_buf = NULL;
	*out_size = 0;
	if (ino->size == 0 || tessera_hash_is_null(ino->manifest_hash))
		return (0);
	uint8_t *buf = malloc((size_t)ino->size, M_TESSERA, M_WAITOK | M_ZERO);

	uint8_t *blob = NULL;
	uint32_t blob_len = 0;
	if (tessera_fs_fetch_blob(tmp_, ino->manifest_hash,
	    &blob, &blob_len) != 0) {
		free(buf, M_TESSERA);
		return (EIO);
	}
	tessera_manifest_parser_t *p = tessera_manifest_parse(blob, blob_len);
	if (p == NULL) {
		free(blob, M_TESSERA);
		free(buf, M_TESSERA);
		return (EIO);
	}
	const tessera_manifest_kind_t kind = tessera_manifest_parser_kind(p);
	int err = 0;
	if (kind == TESSERA_MFT_INLINE) {
		const uint8_t *data = NULL;
		size_t data_len = 0;
		if (tessera_manifest_inline_data(p, &data, &data_len) == TESSERA_OK
		    && data != NULL) {
			if (data_len > (size_t)ino->size) data_len = (size_t)ino->size;
			memcpy(buf, data, data_len);
		}
	} else if (kind == TESSERA_MFT_CHUNK_LIST) {
		const uint32_t n = tessera_manifest_parser_count(p);
		for (uint32_t i = 0; i < n && err == 0; i++) {
			tessera_chunk_record_t cr;
			if (tessera_manifest_chunk_at(p, i, &cr) != TESSERA_OK) {
				err = EIO; break;
			}
			size_t want = cr.uncompressed_size;
			if (cr.logical_offset + want > (uint64_t)ino->size)
				want = (size_t)((uint64_t)ino->size - cr.logical_offset);
			if (cr.flags & TESSERA_CHUNK_FLAG_ZERO_HOLE) {
				/* buf was M_ZERO-allocated — nothing to do. */
				continue;
			}
			uint8_t  *cb = NULL;
			uint32_t  cb_len = 0;
			if (tessera_fs_fetch_blob(tmp_, cr.chunk_hash,
			    &cb, &cb_len) != 0) { err = EIO; break; }
			if (cb_len < want) { free(cb, M_TESSERA); err = EIO; break; }
			memcpy(buf + cr.logical_offset, cb, want);
			free(cb, M_TESSERA);
		}
	} else {
		err = EIO;  /* SYMLINK / DIRECTORY / CHUNK_TREE not handled here */
	}
	tessera_manifest_parser_free(p);
	free(blob, M_TESSERA);
	if (err != 0) { free(buf, M_TESSERA); return (err); }
	*out_buf = buf;
	*out_size = (size_t)ino->size;
	return (0);
}

/*
 * Replace the content of `inode_no`'s file with `new_bytes` (length
 * `new_len`). Builds a fresh INLINE manifest, publishes it, then
 * COW-updates the inode record (manifest_hash + size + ctime/mtime).
 *
 * The caller is responsible for calling tessera_commit_extent +
 * tessera_commit_sb afterwards.
 */
static int
tessera_fs_replace_content(struct tessera_mount *tmp_, uint32_t inode_no,
                           const uint8_t *new_bytes, size_t new_len)
{
	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_INLINE);
	if (mb == NULL) return (ENOMEM);
	if (tessera_manifest_set_inline(mb, new_bytes, new_len) != TESSERA_OK) {
		tessera_manifest_free(mb);
		return (ENOMEM);
	}
	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	uint8_t *mft = malloc(mlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, mft, mlen, &mlen, mhash) != TESSERA_OK) {
		tessera_manifest_free(mb);
		free(mft, M_TESSERA);
		return (EIO);
	}
	tessera_manifest_free(mb);

	tessera_hash_t pub_hash;
	if (tessera_fs_publish_manifest(tmp_, mft, mlen, pub_hash) != 0) {
		free(mft, M_TESSERA);
		return (EIO);
	}
	free(mft, M_TESSERA);

	uint8_t key[4];
	tessera_inode_record_t ino;
	encode_inode_key(inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &ino) != TESSERA_OK)
		return (EIO);
	memcpy(ino.manifest_hash, pub_hash, sizeof pub_hash);
	ino.size = new_len;
	ino.gen++;
	struct timeval tv;
	getmicrotime(&tv);
	uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
	    (uint64_t)tv.tv_usec * 1000ULL;
	ino.mtime_ns = ino.ctime_ns = now_ns;

	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_put(tmp_->inode_tree, key, &ino,
	    &new_inode_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.inode_root = new_inode_root;
	return (0);
}

/*
 * Rewrite a directory's manifest by adding or removing one dirent.
 *
 *   op = 0  → ADD entry (new_inode, name) to the parent.
 *   op = 1  → REMOVE entry (verify_inode, name) from the parent.
 *             For REMOVE, verify_inode is the inode_no the caller
 *             expects — used to detect stale vnodes (returns EIO if
 *             the on-disk dirent disagrees). Use 0 to skip the check.
 *
 * Fetches the parent's current DIRECTORY manifest, builds a fresh one
 * with the requested change, publishes it as a single-blob pack, then
 * COW-updates the parent's inode record (manifest_hash, gen). Caller
 * is responsible for commit_extent + commit_sb afterwards.
 *
 * Returns 0 on success, ENOENT if op==REMOVE and the name isn't
 * found, EEXIST if op==ADD and the name already exists.
 */
static int
tessera_fs_dirent_rewrite(struct tessera_mount *tmp_,
                          uint32_t parent_inode_no,
                          int op, uint64_t verify_inode,
                          uint64_t add_inode,
                          const char *name, size_t namelen)
{
	int err = 0;
	uint8_t  *blob = NULL;
	uint32_t  blob_len = 0;
	uint8_t  *new_mft = NULL;

	uint8_t pkey[4];
	tessera_inode_record_t pino;
	encode_inode_key(parent_inode_no, pkey);
	if (tessera_btree_get(tmp_->inode_tree, pkey, &pino) != TESSERA_OK)
		return (EIO);
	if (tessera_fs_fetch_blob(tmp_, pino.manifest_hash,
	    &blob, &blob_len) != 0)
		return (EIO);
	if (blob_len < 32) { err = EIO; goto out; }
	const uint8_t *body = blob + 32;
	const size_t   blen = blob_len - 32;

	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
	if (mb == NULL) { err = ENOMEM; goto out; }
	int matched = 0;
	for (size_t off = 0; off + 10 <= blen;) {
		uint64_t entry_inode;
		uint16_t entry_nlen;
		memcpy(&entry_inode, body + off,     8);
		memcpy(&entry_nlen,  body + off + 8, 2);
		if (off + 10 + entry_nlen > blen) {
			tessera_manifest_free(mb);
			err = EIO; goto out;
		}
		const char *ename = (const char *)(body + off + 10);
		int match = (entry_nlen == namelen) &&
		    (memcmp(ename, name, namelen) == 0);
		if (match) {
			matched = 1;
			if (op == 0) {
				/* ADD: name already exists — caller's bug. */
				tessera_manifest_free(mb);
				err = EEXIST; goto out;
			}
			/* REMOVE — skip this entry. */
			if (verify_inode != 0 && entry_inode != verify_inode) {
				tessera_manifest_free(mb);
				err = EIO; goto out;
			}
		} else {
			if (tessera_manifest_add_dirent(mb, entry_inode,
			    ename, entry_nlen) != TESSERA_OK) {
				tessera_manifest_free(mb);
				err = ENOMEM; goto out;
			}
		}
		off += 10 + entry_nlen;
	}
	if (op == 1 && !matched) {
		tessera_manifest_free(mb);
		err = ENOENT; goto out;
	}
	if (op == 0) {
		if (tessera_manifest_add_dirent(mb, add_inode, name,
		    namelen) != TESSERA_OK) {
			tessera_manifest_free(mb);
			err = ENOMEM; goto out;
		}
	}

	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	new_mft = malloc(mlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, new_mft, mlen, &mlen,
	    mhash) != TESSERA_OK) {
		tessera_manifest_free(mb);
		err = EIO; goto out;
	}
	tessera_manifest_free(mb);

	tessera_hash_t pub_hash;
	if (tessera_fs_publish_manifest(tmp_, new_mft, mlen,
	    pub_hash) != 0) { err = EIO; goto out; }

	memcpy(pino.manifest_hash, pub_hash, sizeof pub_hash);
	pino.gen++;
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_put(tmp_->inode_tree, pkey, &pino,
	    &new_inode_root) != TESSERA_OK) { err = EIO; goto out; }
	tmp_->sb.inode_root = new_inode_root;

out:
	if (blob)    free(blob,    M_TESSERA);
	if (new_mft) free(new_mft, M_TESSERA);
	return (err);
}

/*
 * vop_create — create a new empty regular file.
 *
 * Plan:
 *   1. Allocate a new inode_no from sb.next_inode_no.
 *   2. Build empty INLINE manifest for the new file; publish it as a
 *      single-blob pack → child_manifest_hash.
 *   3. Compose a new tessera_inode_record_t for the child (mode from
 *      vap->va_mode, size=0, timestamps=now via getmicrotime, linked
 *      to child_manifest_hash). btree_put on inode_tree.
 *   4. Fetch parent's DIRECTORY manifest, build a new DIRECTORY
 *      manifest with the existing entries + a new one for this file.
 *      Publish; btree_put parent inode's new manifest_hash.
 *   5. Allocate child vnode (same shape as vop_lookup).
 *   6. commit_extent + commit_sb.
 *
 * Doesn't yet handle name collisions: it's a v1 thing — VFS layer
 * already runs vop_lookup with NAMEI hooks and refuses CREATE if the
 * name exists.
 */
static int
tessera_vop_create(struct vop_create_args *ap)
{
	struct vnode *dvp = ap->a_dvp;
	struct vnode **vpp = ap->a_vpp;
	struct componentname *cnp = ap->a_cnp;
	struct vattr *vap = ap->a_vap;
	struct tessera_mount *tmp_ = VFSTOTESSERA(dvp->v_mount);
	struct tessera_node  *dn   = VTOTNODE(dvp);

	if (tmp_->inode_tree == NULL) return (EROFS);
	if (cnp->cn_namelen == 0 || cnp->cn_namelen > 0xffff) return (EINVAL);

	int err = 0;
	uint8_t  *blob = NULL;
	uint32_t  blob_len = 0;
	uint8_t  *child_mft = NULL;
	uint8_t  *new_pmft  = NULL;

	/* 1. Allocate inode_no. */
	uint32_t new_ino = (uint32_t)tmp_->sb.next_inode_no;
	if (new_ino < TESSERA_INODE_FIRST_USER) new_ino = TESSERA_INODE_FIRST_USER;
	tmp_->sb.next_inode_no = new_ino + 1;

	/* 2. Build & publish the child's empty INLINE manifest. */
	{
		tessera_manifest_builder_t *mb =
		    tessera_manifest_begin(TESSERA_MFT_INLINE);
		if (mb == NULL) { err = ENOMEM; goto out; }
		if (tessera_manifest_set_inline(mb, NULL, 0) != TESSERA_OK) {
			tessera_manifest_free(mb);
			err = ENOMEM; goto out;
		}
		size_t mlen = 0;
		tessera_hash_t mhash;
		(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
		child_mft = malloc(mlen, M_TESSERA, M_WAITOK);
		if (tessera_manifest_finalize(mb, child_mft, mlen, &mlen,
		    mhash) != TESSERA_OK) {
			tessera_manifest_free(mb);
			err = EIO; goto out;
		}
		tessera_manifest_free(mb);

		tessera_hash_t pub_hash;
		if (tessera_fs_publish_manifest(tmp_, child_mft, mlen,
		    pub_hash) != 0) { err = EIO; goto out; }

		/* 3. Compose & insert child inode record. */
		tessera_inode_record_t cino;
		memset(&cino, 0, sizeof cino);
		cino.inode_no = new_ino;
		cino.gen      = 1;
		cino.mode     = ((vap->va_mode & 07777) | 0100000);  /* S_IFREG */
		cino.uid      = (vap->va_uid != (uid_t)VNOVAL) ? vap->va_uid : 0;
		cino.gid      = (vap->va_gid != (gid_t)VNOVAL) ? vap->va_gid : 0;
		struct timeval tv;
		getmicrotime(&tv);
		uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
		    (uint64_t)tv.tv_usec * 1000ULL;
		cino.atime_ns = cino.mtime_ns = cino.ctime_ns = cino.btime_ns = now_ns;
		cino.size  = 0;
		cino.nlink = 1;
		cino.flags = 0;
		memcpy(cino.manifest_hash, pub_hash, sizeof pub_hash);

		uint8_t ckey[4];
		encode_inode_key(new_ino, ckey);
		uint64_t new_inode_root = tmp_->sb.inode_root;
		if (tessera_btree_put(tmp_->inode_tree, ckey, &cino,
		    &new_inode_root) != TESSERA_OK) { err = EIO; goto out; }
		tmp_->sb.inode_root = new_inode_root;
	}

	/* 4. Rewrite parent DIRECTORY manifest with the new entry. */
	uint8_t pkey[4];
	tessera_inode_record_t pino;
	encode_inode_key((uint32_t)dn->inode_no, pkey);
	if (tessera_btree_get(tmp_->inode_tree, pkey, &pino) != TESSERA_OK) {
		err = EIO; goto out;
	}
	if (tessera_fs_fetch_blob(tmp_, pino.manifest_hash,
	    &blob, &blob_len) != 0) { err = EIO; goto out; }
	if (blob_len < 32) { err = EIO; goto out; }
	const uint8_t *body = blob + 32;
	const size_t   blen = blob_len - 32;

	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
	if (mb == NULL) { err = ENOMEM; goto out; }
	for (size_t off = 0; off + 10 <= blen;) {
		uint64_t entry_inode;
		uint16_t entry_nlen;
		memcpy(&entry_inode, body + off,     8);
		memcpy(&entry_nlen,  body + off + 8, 2);
		if (off + 10 + entry_nlen > blen) {
			tessera_manifest_free(mb);
			err = EIO; goto out;
		}
		if (tessera_manifest_add_dirent(mb, entry_inode,
		    (const char *)(body + off + 10),
		    entry_nlen) != TESSERA_OK) {
			tessera_manifest_free(mb);
			err = ENOMEM; goto out;
		}
		off += 10 + entry_nlen;
	}
	if (tessera_manifest_add_dirent(mb, new_ino, cnp->cn_nameptr,
	    cnp->cn_namelen) != TESSERA_OK) {
		tessera_manifest_free(mb);
		err = ENOMEM; goto out;
	}

	size_t pmlen = 0;
	tessera_hash_t pmhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &pmlen, pmhash);
	new_pmft = malloc(pmlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, new_pmft, pmlen, &pmlen,
	    pmhash) != TESSERA_OK) {
		tessera_manifest_free(mb);
		err = EIO; goto out;
	}
	tessera_manifest_free(mb);

	tessera_hash_t pub_phash;
	if (tessera_fs_publish_manifest(tmp_, new_pmft, pmlen,
	    pub_phash) != 0) { err = EIO; goto out; }

	memcpy(pino.manifest_hash, pub_phash, sizeof pub_phash);
	pino.gen++;
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_put(tmp_->inode_tree, pkey, &pino,
	    &new_inode_root) != TESSERA_OK) { err = EIO; goto out; }
	tmp_->sb.inode_root = new_inode_root;

	/* 5. Get a deduped vnode for the new inode. tessera_vget reads
	 * the just-written inode record, sets v_type=VREG. */
	struct vnode *cvp;
	if (tessera_vget(dvp->v_mount, new_ino, dn->inode_no, &cvp) != 0) {
		err = EIO; goto out;
	}
	*vpp = cvp;

	/* 6. Commit. */
	/* v2-step-2a: SB write is deferred; mark_dirty arms the flush
	 * callout. commit_extent stays synchronous because it advances
	 * tmp_->sb.free_extent_root in lockstep with on-disk btree node
	 * writes that cannot be replayed from the journal. */
	if (tessera_commit_extent(tmp_) != 0) {
		err = EIO; goto out;
	}
	tessera_fs_mark_dirty(tmp_);

out:
	if (blob)      free(blob,      M_TESSERA);
	if (child_mft) free(child_mft, M_TESSERA);
	if (new_pmft)  free(new_pmft,  M_TESSERA);
	return (err);
}

/*
 * vop_write — read-modify-write the file's content.
 *
 *   final_size <= TESSERA_INLINE_THRESHOLD (256 KiB)
 *       INLINE manifest, body holds the bytes (current v1 behaviour).
 *   final_size >  TESSERA_INLINE_THRESHOLD
 *       CHUNK_LIST manifest, body holds chunk records pointing at
 *       TESSERA_CHUNK_SIZE-sized blobs in a multi-blob pack
 *       (v2-step-3a). Cross-file dedup at chunk granularity falls
 *       out for free.
 *
 * Still rewrites the whole file each write — range-aware modification
 * + append fast-path land in step-3b. The cap stops us from running
 * the kernel out of M_WAITOK memory; chunked writes raise it from
 * 1 MiB (v1) to 64 MiB.
 */
#define TESSERA_WRITE_MAX_BYTES   (64u * 1024u * 1024u)

static int
tessera_vop_write(struct vop_write_args *ap)
{
	struct vnode *vp = ap->a_vp;
	struct uio   *uio = ap->a_uio;
	int           ioflag = ap->a_ioflag;
	struct tessera_node *tn = VTOTNODE(vp);
	struct tessera_mount *tmp_ = VFSTOTESSERA(vp->v_mount);

	if (vp->v_type == VDIR) return (EISDIR);
	if (uio->uio_resid == 0) return (0);
	if (uio->uio_offset < 0) return (EINVAL);
	if (tmp_->inode_tree == NULL) return (EROFS);

	/* Read live inode record. */
	uint8_t key[4];
	tessera_inode_record_t ino;
	encode_inode_key((uint32_t)tn->inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &ino) != TESSERA_OK)
		return (EIO);

	if (ioflag & IO_APPEND)
		uio->uio_offset = (off_t)ino.size;

	const uint64_t write_off  = (uint64_t)uio->uio_offset;
	const uint64_t write_resid = (uint64_t)uio->uio_resid;
	const uint64_t write_end  = write_off + write_resid;
	const uint64_t final_size = write_end > ino.size ? write_end : ino.size;
	if (final_size > TESSERA_WRITE_MAX_BYTES)
		return (EFBIG);

	/* Drain the uio into a kernel buffer of just the new bytes (not
	 * the whole file). Both fast and slow paths reuse this; it
	 * separates "consume userspace" from "decide which path". */
	uint8_t *new_bytes = malloc((size_t)write_resid, M_TESSERA, M_WAITOK);
	int err = uiomove(new_bytes, (int)write_resid, uio);
	if (err != 0) {
		free(new_bytes, M_TESSERA);
		return (err);
	}

	/* Append fast-path (step-3b): pure append into a chunked file
	 * skips materialising the existing bytes entirely. Eligibility
	 * checks live in tessera_fs_append_chunked; on ENOTSUP we fall
	 * through to the slow read-modify-write path below. */
	if (write_off == ino.size &&
	    final_size > TESSERA_INLINE_THRESHOLD) {
		const uint32_t cs = tessera_chunk_size_for(final_size);
		int frc = tessera_fs_append_chunked(tmp_,
		    (uint32_t)tn->inode_no, new_bytes,
		    (size_t)write_resid, cs);
		if (frc == 0) {
			tessera_stat_append_fast_ok++;
			tessera_stat_vop_write_chunked++;
			free(new_bytes, M_TESSERA);
			if (tessera_commit_extent(tmp_) != 0) return (EIO);
			tessera_fs_mark_dirty(tmp_);
			return (0);
		}
		if (frc != ENOTSUP) {
			free(new_bytes, M_TESSERA);
			return (frc);
		}
		tessera_stat_append_fast_fallback++;
		/* Fall through to slow path. */
	}

	/* Slow path: materialise the existing content, splice in the
	 * new bytes, route through INLINE or whole-file chunked
	 * rewrite. */
	uint8_t *old_buf = NULL;
	size_t   old_len = 0;
	if (tessera_fs_read_full_content(tmp_, &ino, &old_buf, &old_len) != 0) {
		free(new_bytes, M_TESSERA);
		return (EIO);
	}
	uint8_t *full = malloc((size_t)final_size, M_TESSERA,
	    M_WAITOK | M_ZERO);
	if (old_buf != NULL) {
		size_t n = old_len < (size_t)final_size ? old_len
		    : (size_t)final_size;
		memcpy(full, old_buf, n);
		free(old_buf, M_TESSERA);
	}
	memcpy(full + write_off, new_bytes, (size_t)write_resid);
	free(new_bytes, M_TESSERA);

	int rc;
	if ((size_t)final_size <= TESSERA_INLINE_THRESHOLD) {
		rc = tessera_fs_replace_content(tmp_,
		    (uint32_t)tn->inode_no, full, (size_t)final_size);
		tessera_stat_vop_write_inline++;
	} else {
		rc = tessera_fs_replace_content_chunked(tmp_,
		    (uint32_t)tn->inode_no, full, (size_t)final_size);
		tessera_stat_vop_write_chunked++;
	}
	free(full, M_TESSERA);
	if (rc != 0) return (rc);

	if (tessera_commit_extent(tmp_) != 0)
		return (EIO);
	tessera_fs_mark_dirty(tmp_);
	return (0);
}

/*
 * vop_mkdir — create a new empty subdirectory.
 *
 * Same shape as vop_create but the child gets:
 *   - mode = 040000 | (vap->va_mode & 07777)            (S_IFDIR)
 *   - manifest = empty DIRECTORY manifest (no dirents)
 *   - nlink = 2 (parent's dirent + the implicit "." we don't store)
 * and v_type = VDIR.
 */
static int
tessera_vop_mkdir(struct vop_mkdir_args *ap)
{
	struct vnode *dvp = ap->a_dvp;
	struct vnode **vpp = ap->a_vpp;
	struct componentname *cnp = ap->a_cnp;
	struct vattr *vap = ap->a_vap;
	struct tessera_mount *tmp_ = VFSTOTESSERA(dvp->v_mount);
	struct tessera_node  *dn   = VTOTNODE(dvp);

	if (tmp_->inode_tree == NULL) return (EROFS);
	if (cnp->cn_namelen == 0 || cnp->cn_namelen > 0xffff) return (EINVAL);

	int err = 0;
	uint8_t *child_mft = NULL;

	uint32_t new_ino = (uint32_t)tmp_->sb.next_inode_no;
	if (new_ino < TESSERA_INODE_FIRST_USER) new_ino = TESSERA_INODE_FIRST_USER;
	tmp_->sb.next_inode_no = new_ino + 1;

	/* Empty DIRECTORY manifest. */
	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
	if (mb == NULL) { err = ENOMEM; goto out; }
	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	child_mft = malloc(mlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, child_mft, mlen, &mlen,
	    mhash) != TESSERA_OK) {
		tessera_manifest_free(mb);
		err = EIO; goto out;
	}
	tessera_manifest_free(mb);

	tessera_hash_t pub_hash;
	if (tessera_fs_publish_manifest(tmp_, child_mft, mlen,
	    pub_hash) != 0) { err = EIO; goto out; }

	tessera_inode_record_t cino;
	memset(&cino, 0, sizeof cino);
	cino.inode_no = new_ino;
	cino.gen      = 1;
	cino.mode     = ((vap->va_mode & 07777) | 0040000);  /* S_IFDIR */
	cino.uid      = (vap->va_uid != (uid_t)VNOVAL) ? vap->va_uid : 0;
	cino.gid      = (vap->va_gid != (gid_t)VNOVAL) ? vap->va_gid : 0;
	struct timeval tv;
	getmicrotime(&tv);
	uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
	    (uint64_t)tv.tv_usec * 1000ULL;
	cino.atime_ns = cino.mtime_ns = cino.ctime_ns = cino.btime_ns = now_ns;
	cino.size  = 0;
	cino.nlink = 2;
	cino.flags = 0;
	memcpy(cino.manifest_hash, pub_hash, sizeof pub_hash);

	uint8_t ckey[4];
	encode_inode_key(new_ino, ckey);
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_put(tmp_->inode_tree, ckey, &cino,
	    &new_inode_root) != TESSERA_OK) { err = EIO; goto out; }
	tmp_->sb.inode_root = new_inode_root;

	/* Add dirent into parent. */
	err = tessera_fs_dirent_rewrite(tmp_, (uint32_t)dn->inode_no,
	    /*op*/ 0, /*verify*/ 0, /*add_inode*/ new_ino,
	    cnp->cn_nameptr, cnp->cn_namelen);
	if (err != 0) goto out;

	/* Get a deduped vnode for the new dir. */
	struct vnode *cvp;
	if (tessera_vget(dvp->v_mount, new_ino, dn->inode_no, &cvp) != 0) {
		err = EIO; goto out;
	}
	*vpp = cvp;

	/* v2-step-2a: SB write is deferred; mark_dirty arms the flush
	 * callout. commit_extent stays synchronous because it advances
	 * tmp_->sb.free_extent_root in lockstep with on-disk btree node
	 * writes that cannot be replayed from the journal. */
	if (tessera_commit_extent(tmp_) != 0) {
		err = EIO; goto out;
	}
	tessera_fs_mark_dirty(tmp_);

out:
	if (child_mft) free(child_mft, M_TESSERA);
	return (err);
}

/*
 * vop_rmdir — unlink an empty subdirectory.
 *
 * Differs from vop_remove only by enforcing emptiness: fetch the
 * child's DIRECTORY manifest and refuse with ENOTEMPTY if it has any
 * dirents.
 */
static int
tessera_vop_rmdir(struct vop_rmdir_args *ap)
{
	struct vnode *dvp = ap->a_dvp;
	struct vnode *vp  = ap->a_vp;
	struct componentname *cnp = ap->a_cnp;
	struct tessera_mount *tmp_ = VFSTOTESSERA(dvp->v_mount);
	struct tessera_node  *dn = VTOTNODE(dvp);
	struct tessera_node  *cn = VTOTNODE(vp);

	if (tmp_->inode_tree == NULL) return (EROFS);

	/* Fetch child inode + manifest, verify it's empty. */
	uint8_t ckey[4];
	tessera_inode_record_t cino;
	encode_inode_key((uint32_t)cn->inode_no, ckey);
	if (tessera_btree_get(tmp_->inode_tree, ckey, &cino) != TESSERA_OK)
		return (EIO);
	if ((cino.mode & 0170000) != 0040000)
		return (ENOTDIR);
	uint8_t *cblob = NULL;
	uint32_t cblob_len = 0;
	if (!tessera_hash_is_null(cino.manifest_hash)) {
		if (tessera_fs_fetch_blob(tmp_, cino.manifest_hash,
		    &cblob, &cblob_len) != 0)
			return (EIO);
		if (cblob_len > 32) {
			free(cblob, M_TESSERA);
			return (ENOTEMPTY);
		}
		free(cblob, M_TESSERA);
	}

	/* Remove dirent from parent. */
	int err = tessera_fs_dirent_rewrite(tmp_, (uint32_t)dn->inode_no,
	    /*op*/ 1, /*verify*/ cn->inode_no, /*add*/ 0,
	    cnp->cn_nameptr, cnp->cn_namelen);
	if (err != 0) return (err);

	/* Delete child inode record. */
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_delete(tmp_->inode_tree, ckey,
	    &new_inode_root) != TESSERA_OK)
		printf("tessera_fs: vop_rmdir — btree_delete child "
		    "inode=%u failed\n", (unsigned)cn->inode_no);
	else
		tmp_->sb.inode_root = new_inode_root;

	if (tessera_commit_extent(tmp_) != 0)
		return (EIO);
	tessera_fs_mark_dirty(tmp_);
	return (0);
}

/*
 * vop_symlink — create a symlink whose target is `ap->a_target`.
 *
 * Allocates a fresh inode (mode = S_IFLNK | 0777), publishes a
 * SYMLINK manifest containing the target string, adds the dirent to
 * the parent. Same pattern as vop_create + vop_mkdir.
 */
static int
tessera_vop_symlink(struct vop_symlink_args *ap)
{
	struct vnode *dvp = ap->a_dvp;
	struct vnode **vpp = ap->a_vpp;
	struct componentname *cnp = ap->a_cnp;
	const char *target = ap->a_target;
	struct tessera_mount *tmp_ = VFSTOTESSERA(dvp->v_mount);
	struct tessera_node  *dn   = VTOTNODE(dvp);

	if (tmp_->inode_tree == NULL) return (EROFS);
	if (cnp->cn_namelen == 0 || cnp->cn_namelen > 0xffff) return (EINVAL);
	if (target == NULL) return (EINVAL);
	size_t tlen = strlen(target);
	if (tlen == 0 || tlen > 4096) return (ENAMETOOLONG);

	int err = 0;
	uint8_t *child_mft = NULL;

	uint32_t new_ino = (uint32_t)tmp_->sb.next_inode_no;
	if (new_ino < TESSERA_INODE_FIRST_USER) new_ino = TESSERA_INODE_FIRST_USER;
	tmp_->sb.next_inode_no = new_ino + 1;

	/* Build SYMLINK manifest. */
	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_SYMLINK);
	if (mb == NULL) { err = ENOMEM; goto out; }
	if (tessera_manifest_set_symlink(mb, target) != TESSERA_OK) {
		tessera_manifest_free(mb);
		err = ENOMEM; goto out;
	}
	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	child_mft = malloc(mlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, child_mft, mlen, &mlen,
	    mhash) != TESSERA_OK) {
		tessera_manifest_free(mb);
		err = EIO; goto out;
	}
	tessera_manifest_free(mb);

	tessera_hash_t pub_hash;
	if (tessera_fs_publish_manifest(tmp_, child_mft, mlen,
	    pub_hash) != 0) { err = EIO; goto out; }

	tessera_inode_record_t cino;
	memset(&cino, 0, sizeof cino);
	cino.inode_no = new_ino;
	cino.gen      = 1;
	cino.mode     = 0120777;     /* S_IFLNK | 0777 */
	struct timeval tv;
	getmicrotime(&tv);
	uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
	    (uint64_t)tv.tv_usec * 1000ULL;
	cino.atime_ns = cino.mtime_ns = cino.ctime_ns = cino.btime_ns = now_ns;
	cino.size  = tlen;
	cino.nlink = 1;
	memcpy(cino.manifest_hash, pub_hash, sizeof pub_hash);

	uint8_t ckey[4];
	encode_inode_key(new_ino, ckey);
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_put(tmp_->inode_tree, ckey, &cino,
	    &new_inode_root) != TESSERA_OK) { err = EIO; goto out; }
	tmp_->sb.inode_root = new_inode_root;

	err = tessera_fs_dirent_rewrite(tmp_, (uint32_t)dn->inode_no,
	    /*op*/ 0, /*verify*/ 0, /*add*/ new_ino,
	    cnp->cn_nameptr, cnp->cn_namelen);
	if (err != 0) goto out;

	struct vnode *cvp;
	if (tessera_vget(dvp->v_mount, new_ino, dn->inode_no, &cvp) != 0) {
		err = EIO; goto out;
	}
	*vpp = cvp;

	/* v2-step-2a: SB write is deferred; mark_dirty arms the flush
	 * callout. commit_extent stays synchronous because it advances
	 * tmp_->sb.free_extent_root in lockstep with on-disk btree node
	 * writes that cannot be replayed from the journal. */
	if (tessera_commit_extent(tmp_) != 0) {
		err = EIO; goto out;
	}
	tessera_fs_mark_dirty(tmp_);

out:
	if (child_mft) free(child_mft, M_TESSERA);
	return (err);
}

/*
 * vop_readlink — copy the symlink target out via uio.
 */
static int
tessera_vop_readlink(struct vop_readlink_args *ap)
{
	struct vnode *vp = ap->a_vp;
	struct uio   *uio = ap->a_uio;
	struct tessera_node *tn = VTOTNODE(vp);
	struct tessera_mount *tmp_ = VFSTOTESSERA(vp->v_mount);

	uint8_t key[4];
	tessera_inode_record_t ino;
	encode_inode_key((uint32_t)tn->inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &ino) != TESSERA_OK)
		return (EIO);
	if ((ino.mode & 0170000) != 0120000) return (EINVAL);

	uint8_t *blob = NULL;
	uint32_t blob_len = 0;
	if (tessera_fs_fetch_blob(tmp_, ino.manifest_hash, &blob,
	    &blob_len) != 0)
		return (EIO);
	tessera_manifest_parser_t *p = tessera_manifest_parse(blob, blob_len);
	if (p == NULL) {
		free(blob, M_TESSERA);
		return (EIO);
	}
	const uint8_t *data = NULL;
	size_t data_len = 0;
	int err = 0;
	if (tessera_manifest_inline_data(p, &data, &data_len) != TESSERA_OK ||
	    data == NULL)
		err = EIO;
	else
		err = uiomove(__DECONST(void *, data), (int)data_len, uio);
	tessera_manifest_parser_free(p);
	free(blob, M_TESSERA);
	return (err);
}

/*
 * vop_link — hard link. Adds another dirent pointing at the existing
 * file's inode and bumps its nlink count. Refuses on directories
 * (POSIX); refuses across mounts (VFS already gates).
 */
static int
tessera_vop_link(struct vop_link_args *ap)
{
	struct vnode *tdvp = ap->a_tdvp;
	struct vnode *vp   = ap->a_vp;
	struct componentname *cnp = ap->a_cnp;
	struct tessera_mount *tmp_ = VFSTOTESSERA(tdvp->v_mount);
	struct tessera_node  *dn = VTOTNODE(tdvp);
	struct tessera_node  *cn = VTOTNODE(vp);

	if (tmp_->inode_tree == NULL) return (EROFS);
	if (vp->v_type == VDIR) return (EPERM);
	if (tdvp->v_mount != vp->v_mount) return (EXDEV);

	uint8_t ckey[4];
	tessera_inode_record_t cino;
	encode_inode_key((uint32_t)cn->inode_no, ckey);
	if (tessera_btree_get(tmp_->inode_tree, ckey, &cino) != TESSERA_OK)
		return (EIO);
	cino.nlink++;
	struct timeval tv;
	getmicrotime(&tv);
	cino.ctime_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
	    (uint64_t)tv.tv_usec * 1000ULL;
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_put(tmp_->inode_tree, ckey, &cino,
	    &new_inode_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.inode_root = new_inode_root;

	int err = tessera_fs_dirent_rewrite(tmp_, (uint32_t)dn->inode_no,
	    /*op*/ 0, /*verify*/ 0, /*add*/ cn->inode_no,
	    cnp->cn_nameptr, cnp->cn_namelen);
	if (err != 0) {
		/* Roll back nlink bump on dirent failure. */
		cino.nlink--;
		(void)tessera_btree_put(tmp_->inode_tree, ckey, &cino,
		    &new_inode_root);
		tmp_->sb.inode_root = new_inode_root;
		return (err);
	}

	if (tessera_commit_extent(tmp_) != 0)
		return (EIO);
	tessera_fs_mark_dirty(tmp_);
	return (0);
}

/*
 * In-place same-dir rename: rewrite `parent`'s DIRECTORY manifest so
 * that the dirent for (old_name, old_namelen) is replaced with
 * (new_name, new_namelen) — both naming the same `target_inode`. The
 * single rewrite avoids any in-between state with two-or-zero copies
 * of the entry. Caller does commit_extent + commit_sb.
 */
static int
tessera_fs_dirent_rename_same_dir(struct tessera_mount *tmp_,
    uint32_t parent_inode_no,
    uint64_t target_inode,
    const char *old_name, size_t old_namelen,
    const char *new_name, size_t new_namelen)
{
	int err = 0;
	uint8_t  *blob = NULL;
	uint32_t  blob_len = 0;
	uint8_t  *new_mft = NULL;

	uint8_t pkey[4];
	tessera_inode_record_t pino;
	encode_inode_key(parent_inode_no, pkey);
	if (tessera_btree_get(tmp_->inode_tree, pkey, &pino) != TESSERA_OK)
		return (EIO);
	if (tessera_fs_fetch_blob(tmp_, pino.manifest_hash, &blob,
	    &blob_len) != 0)
		return (EIO);
	if (blob_len < 32) { err = EIO; goto out; }
	const uint8_t *body = blob + 32;
	const size_t   blen = blob_len - 32;

	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
	if (mb == NULL) { err = ENOMEM; goto out; }
	int matched = 0, conflict = 0;
	for (size_t off = 0; off + 10 <= blen;) {
		uint64_t entry_inode;
		uint16_t entry_nlen;
		memcpy(&entry_inode, body + off,     8);
		memcpy(&entry_nlen,  body + off + 8, 2);
		if (off + 10 + entry_nlen > blen) {
			tessera_manifest_free(mb);
			err = EIO; goto out;
		}
		const char *ename = (const char *)(body + off + 10);
		int is_old = (entry_nlen == old_namelen) &&
		    (memcmp(ename, old_name, old_namelen) == 0);
		int is_new = (entry_nlen == new_namelen) &&
		    (memcmp(ename, new_name, new_namelen) == 0);
		if (is_old) {
			matched = 1;
			if (entry_inode != target_inode) {
				tessera_manifest_free(mb);
				err = EIO; goto out;
			}
			/* drop */
		} else if (is_new) {
			conflict = 1;
			/* keep walking so the loop terminates cleanly, but
			 * we'll bail below */
			if (tessera_manifest_add_dirent(mb, entry_inode,
			    ename, entry_nlen) != TESSERA_OK) {
				tessera_manifest_free(mb);
				err = ENOMEM; goto out;
			}
		} else {
			if (tessera_manifest_add_dirent(mb, entry_inode,
			    ename, entry_nlen) != TESSERA_OK) {
				tessera_manifest_free(mb);
				err = ENOMEM; goto out;
			}
		}
		off += 10 + entry_nlen;
	}
	if (!matched) {
		tessera_manifest_free(mb);
		err = ENOENT; goto out;
	}
	if (conflict) {
		tessera_manifest_free(mb);
		err = EEXIST; goto out;
	}
	if (tessera_manifest_add_dirent(mb, target_inode, new_name,
	    new_namelen) != TESSERA_OK) {
		tessera_manifest_free(mb);
		err = ENOMEM; goto out;
	}

	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	new_mft = malloc(mlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, new_mft, mlen, &mlen,
	    mhash) != TESSERA_OK) {
		tessera_manifest_free(mb);
		err = EIO; goto out;
	}
	tessera_manifest_free(mb);

	tessera_hash_t pub_hash;
	if (tessera_fs_publish_manifest(tmp_, new_mft, mlen,
	    pub_hash) != 0) { err = EIO; goto out; }

	memcpy(pino.manifest_hash, pub_hash, sizeof pub_hash);
	pino.gen++;
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_btree_put(tmp_->inode_tree, pkey, &pino,
	    &new_inode_root) != TESSERA_OK) { err = EIO; goto out; }
	tmp_->sb.inode_root = new_inode_root;

out:
	if (blob)    free(blob,    M_TESSERA);
	if (new_mft) free(new_mft, M_TESSERA);
	return (err);
}

/*
 * Decrement nlink on `inode_no` (read inode record, dec, write back).
 * If nlink reaches 0, delete the inode record entirely. Caller is
 * responsible for commit_extent + commit_sb. The data-zone blobs
 * referenced by the dropped inode are NOT freed — they leak until
 * offline GC sweeps.
 */
static int
tessera_fs_inode_unlink(struct tessera_mount *tmp_, uint32_t inode_no)
{
	uint8_t key[4];
	tessera_inode_record_t ino;
	encode_inode_key(inode_no, key);
	if (tessera_btree_get(tmp_->inode_tree, key, &ino) != TESSERA_OK)
		return (EIO);
	if (ino.nlink > 1) {
		ino.nlink--;
		struct timeval tv;
		getmicrotime(&tv);
		ino.ctime_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
		    (uint64_t)tv.tv_usec * 1000ULL;
		uint64_t new_root = tmp_->sb.inode_root;
		if (tessera_btree_put(tmp_->inode_tree, key, &ino,
		    &new_root) != TESSERA_OK)
			return (EIO);
		tmp_->sb.inode_root = new_root;
		return (0);
	}
	uint64_t new_root = tmp_->sb.inode_root;
	if (tessera_btree_delete(tmp_->inode_tree, key,
	    &new_root) != TESSERA_OK)
		printf("tessera_fs: inode_unlink — btree_delete inode=%u "
		    "failed\n", (unsigned)inode_no);
	else
		tmp_->sb.inode_root = new_root;
	return (0);
}

/*
 * vop_rename — minimal v1 implementation.
 *
 * Lock contract (FreeBSD WILLRELE): fdvp + fvp arrive REFERENCED but
 * UNLOCKED; tdvp + tvp (if non-NULL) arrive REFERENCED and LOCKED
 * EXCLUSIVE. The callee must release all of them.
 *
 * v1 supported cases: same-directory rename, target name not already
 * present. Cross-dir and overwrite-rename return EOPNOTSUPP — the
 * shell falls back to copy+remove for those.
 */
static int
tessera_vop_rename(struct vop_rename_args *ap)
{
	struct vnode *fdvp = ap->a_fdvp;
	struct vnode *fvp  = ap->a_fvp;
	struct componentname *fcnp = ap->a_fcnp;
	struct vnode *tdvp = ap->a_tdvp;
	struct vnode *tvp  = ap->a_tvp;
	struct componentname *tcnp = ap->a_tcnp;

	int err = 0;

	if (fdvp->v_mount != tdvp->v_mount) {
		err = EXDEV;
		goto release;
	}
	struct tessera_mount *tmp_ = VFSTOTESSERA(fdvp->v_mount);
	struct tessera_node  *fdn  = VTOTNODE(fdvp);
	struct tessera_node  *tdn  = VTOTNODE(tdvp);
	struct tessera_node  *fn   = VTOTNODE(fvp);
	struct tessera_node  *tn   = (tvp != NULL) ? VTOTNODE(tvp) : NULL;
	if (tvp != NULL) {
		/* POSIX type matching: regular ↔ regular, dir ↔ empty-dir. */
		if ((fvp->v_type == VDIR) != (tvp->v_type == VDIR)) {
			err = ((fvp->v_type == VDIR) ? ENOTDIR : EISDIR);
			goto release;
		}
		if (tvp->v_type == VDIR) {
			/* Target dir must be empty. */
			uint8_t tkey[4];
			tessera_inode_record_t tino;
			encode_inode_key((uint32_t)tn->inode_no, tkey);
			if (tessera_btree_get(tmp_->inode_tree, tkey, &tino)
			    != TESSERA_OK) { err = EIO; goto release; }
			uint8_t *tblob = NULL;
			uint32_t tblob_len = 0;
			if (!tessera_hash_is_null(tino.manifest_hash) &&
			    tessera_fs_fetch_blob(tmp_, tino.manifest_hash,
			    &tblob, &tblob_len) == 0) {
				int empty = (tblob_len <= 32);
				free(tblob, M_TESSERA);
				if (!empty) { err = ENOTEMPTY; goto release; }
			}
		}
	}
	/* Compare inode_no, not vnode pointers — our lookup allocates a
	 * fresh vnode per call, so the same on-disk dir produces distinct
	 * vnode pointers. */
	int same_parent = (fdn->inode_no == tdn->inode_no);
	if (same_parent &&
	    fcnp->cn_namelen == tcnp->cn_namelen &&
	    memcmp(fcnp->cn_nameptr, tcnp->cn_nameptr, fcnp->cn_namelen) == 0)
		goto release;            /* identical name — silent no-op */

	if (same_parent && tvp == NULL) {
		err = tessera_fs_dirent_rename_same_dir(tmp_,
		    (uint32_t)fdn->inode_no, fn->inode_no,
		    fcnp->cn_nameptr, fcnp->cn_namelen,
		    tcnp->cn_nameptr, tcnp->cn_namelen);
	} else {
		/* Cross-dir, or same-dir overwrite. Order:
		 *   1. If overwrite: REMOVE the existing target dirent.
		 *   2. Add (fn, tcnp) to target dir.
		 *   3. Remove (fcnp, fn) from source dir.
		 *   4. If overwrite: drop nlink on the displaced inode.
		 * Same-dir overwrite collapses 1+2+3 into one parent rewrite
		 * — handled by going through dirent_rewrite REMOVE then ADD,
		 * which costs an extra commit but is correct.
		 */
		if (tvp != NULL) {
			err = tessera_fs_dirent_rewrite(tmp_,
			    (uint32_t)tdn->inode_no,
			    /*op*/ 1, /*verify*/ tn->inode_no, /*add*/ 0,
			    tcnp->cn_nameptr, tcnp->cn_namelen);
			if (err != 0) goto release;
		}
		err = tessera_fs_dirent_rewrite(tmp_, (uint32_t)tdn->inode_no,
		    /*op*/ 0, /*verify*/ 0, /*add*/ fn->inode_no,
		    tcnp->cn_nameptr, tcnp->cn_namelen);
		if (err != 0) goto release;
		err = tessera_fs_dirent_rewrite(tmp_, (uint32_t)fdn->inode_no,
		    /*op*/ 1, /*verify*/ fn->inode_no, /*add*/ 0,
		    fcnp->cn_nameptr, fcnp->cn_namelen);
		if (err != 0) {
			printf("tessera_fs: rename — REMOVE-from-source failed "
			    "(%d); duplicate dirent left for fsck\n", err);
			err = 0;
		}
		if (tvp != NULL) {
			(void)tessera_fs_inode_unlink(tmp_,
			    (uint32_t)tn->inode_no);
		}
	}
	if (err != 0) goto release;

	if (tessera_commit_extent(tmp_) != 0)
		err = EIO;
	else
		tessera_fs_mark_dirty(tmp_);

release:
	/* fdvp + fvp came UNLOCKED → vrele only */
	if (tvp != NULL) vput(tvp);
	vput(tdvp);
	vrele(fdvp);
	vrele(fvp);
	return (err);
}

struct vop_vector tessera_vnodeops = {
	.vop_default  = &default_vnodeops,
	.vop_access   = tessera_vop_access,
	.vop_getattr  = tessera_vop_getattr,
	.vop_setattr  = tessera_vop_setattr,
	.vop_lookup   = tessera_vop_lookup,
	.vop_readdir  = tessera_vop_readdir,
	.vop_read     = tessera_vop_read,
	.vop_write    = tessera_vop_write,
	.vop_create   = tessera_vop_create,
	.vop_mkdir    = tessera_vop_mkdir,
	.vop_remove   = tessera_vop_remove,
	.vop_rmdir    = tessera_vop_rmdir,
	.vop_symlink  = tessera_vop_symlink,
	.vop_readlink = tessera_vop_readlink,
	.vop_link     = tessera_vop_link,
	.vop_rename   = tessera_vop_rename,
	.vop_open     = tessera_vop_open,
	.vop_close    = tessera_vop_close,
	.vop_fsync    = tessera_vop_fsync,
	.vop_reclaim  = tessera_vop_reclaim,
};
VFS_VOP_VECTOR_REGISTER(tessera_vnodeops);

VFS_SET(tessera_vfsops, tessera, 0);
MODULE_VERSION(tessera_fs, 1);
