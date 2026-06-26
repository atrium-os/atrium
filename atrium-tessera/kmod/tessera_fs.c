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
#include <sys/jail.h>
#include <sys/proc.h>
#include <sys/sysctl.h>
#include <sys/priv.h>
#include <sys/malloc.h>
#include <sys/uio.h>
#include <sys/types.h>
#include <sys/buf.h>
#include <sys/conf.h>
#include <sys/priv.h>
#include <sys/fcntl.h>
#include <sys/callout.h>
#include <sys/lock.h>
#include <sys/mutex.h>
#include <sys/rwlock.h>
#include <sys/taskqueue.h>
#include <sys/unistd.h>
#include <sys/limits.h>
#include <sys/bio.h>
#include <geom/geom.h>
#include <geom/geom_vfs.h>
#include <vm/vm.h>
#include <vm/vm_extern.h>
#include <vm/vm_object.h>
#include <vm/vm_page.h>
#include <vm/vm_pager.h>
#include <vm/vnode_pager.h>
#include <sys/sf_buf.h>

#include "tessera/btree.h"
#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera/extent.h"
#include "tessera/format.h"
#include "tessera/hash.h"
#include "tessera/quota.h"
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
/* Singleton ref to the most-recently-mounted tessera. Used by debug /
 * test sysctls (kern.tessera.repack_one) that need to reach the live
 * mount without iterating mountlist. NULL when nothing's mounted. */
static struct tessera_mount *tessera_singleton_mount = NULL;

/* Debug knob: when non-zero, tessera_fs_pack_alloc_and_write skips the
 * contig fast path and goes straight to the multi-extent allocator.
 * Used to deterministically create MULTI_EXTENT packs for repack tests
 * without having to organically fragment the data zone first. */
static int tessera_force_multi_extent = 0;
/* C2/C3 — background trigger + mount-time safety net thresholds.
 * Declared early so mark_dirty (line ~3400) and mountfs (line ~1350)
 * can see them; the SYSCTL_INT macros that expose them are deeper in
 * the file. */
static int tessera_repack_threshold = 50;
static int tessera_repack_severe_threshold = 500;
static int tessera_repack_bg_max_packs = 5;
static int tessera_repack_bg_max_time_ms = 100;
static int tessera_repack_mount_max_packs = 100;
static int tessera_repack_mount_max_time_ms = 1000;
SYSCTL_INT(_kern_tessera, OID_AUTO, force_multi_extent,
    CTLFLAG_RW, &tessera_force_multi_extent, 0,
    "Force pack allocator to take the multi-extent fallback path (debug)");

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

/* v2 slice-4 debug tooling: meta-reserve trace ring. Records every
 * meta_alloc / meta_free / drain-* event with (op, sector, gen, count)
 * so a hung VM's dmesg-via-sysctl can show what the meta-reserve was
 * doing right before the hang. Pure memory writes — no locks, single
 * monotonic write index. Reads from sysctl are racy by design (best-
 * effort post-mortem inspection). Static (kmod-lifetime) so traces
 * survive umount; useful when slice-4-related corruption only
 * manifests on a re-mount cycles later.
 *
 * Disabled by default (`metatrace_enabled = 0`) so production paths
 * don't pay the write cost. Set via `sysctl kern.tessera.metatrace_enabled=1`. */
enum tessera_metatrace_op {
	TM_OP_ALLOC_BUMP   = 1,  /* meta_alloc satisfied by bump pointer */
	TM_OP_ALLOC_REUSE  = 2,  /* meta_alloc satisfied from meta_free */
	TM_OP_FREE_PUSH    = 3,  /* sector pushed onto meta_pending */
	TM_OP_DRAIN_BEGIN  = 4,  /* commit_sb drain step begins */
	TM_OP_DRAIN_KEEP   = 5,  /* drain kept a sector (pinned by snapshot) */
	TM_OP_DRAIN_RELEASE= 6,  /* drain released a sector to meta_free */
	TM_OP_DRAIN_END    = 7,  /* commit_sb drain step ends */
	TM_OP_SNAPSHOT_REC = 8,  /* commit_sb appended a snapshot record */
	TM_OP_BITMAP_BUILT = 9,  /* mount-time meta-mark walk completed */
	TM_OP_BITMAP_HIT   = 10, /* drain checked bitmap, bit was SET */
	TM_OP_BITMAP_MISS  = 11, /* drain checked bitmap, bit was CLEAR */
};
struct tessera_metatrace_entry {
	uint64_t seq;          /* monotonic; 0 means unused slot */
	uint64_t sector;
	uint64_t gen;
	uint32_t count;        /* meta_pending_count or meta_free_count, op-specific */
	uint8_t  op;
	uint8_t  _pad[3];
};
#define TESSERA_METATRACE_LEN  1024u
static struct tessera_metatrace_entry tessera_metatrace_ring[TESSERA_METATRACE_LEN];
static unsigned long tessera_metatrace_widx = 0;     /* monotonic write index */
static int           tessera_metatrace_enabled = 0;
SYSCTL_INT(_kern_tessera, OID_AUTO, metatrace_enabled,
    CTLFLAG_RW, &tessera_metatrace_enabled, 0,
    "Enable meta-reserve event tracing (1=on, 0=off)");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, metatrace_widx,
    CTLFLAG_RD, &tessera_metatrace_widx, 0,
    "Total meta-reserve trace events recorded since kmod load");

static inline void
tessera_metatrace(uint8_t op, uint64_t sector, uint64_t gen, uint32_t count)
{
	if (!tessera_metatrace_enabled) return;
	unsigned long i = atomic_fetchadd_long(&tessera_metatrace_widx, 1);
	struct tessera_metatrace_entry *e =
	    &tessera_metatrace_ring[i % TESSERA_METATRACE_LEN];
	e->seq    = i + 1;  /* seq=0 means "never written" */
	e->sector = sector;
	e->gen    = gen;
	e->count  = count;
	e->op     = op;
}

/* sysctl handler that prints the last N events to dmesg. Reading the
 * sysctl from userspace triggers the print; the value returned is just
 * the current write index. Awkward but minimal — avoids variable-length
 * sysctl plumbing. Print order is oldest-to-newest within the ring. */
static int
tessera_sysctl_metatrace_dump(SYSCTL_HANDLER_ARGS)
{
	unsigned long widx = tessera_metatrace_widx;
	const unsigned long start = (widx > TESSERA_METATRACE_LEN)
	    ? (widx - TESSERA_METATRACE_LEN) : 0;
	printf("tessera_fs: metatrace dump (widx=%lu, %lu entries):\n",
	    widx, widx - start);
	for (unsigned long i = start; i < widx; i++) {
		const struct tessera_metatrace_entry *e =
		    &tessera_metatrace_ring[i % TESSERA_METATRACE_LEN];
		if (e->seq == 0) continue;
		const char *opn;
		switch (e->op) {
		case TM_OP_ALLOC_BUMP:    opn = "alloc-bump";    break;
		case TM_OP_ALLOC_REUSE:   opn = "alloc-reuse";   break;
		case TM_OP_FREE_PUSH:     opn = "free-push";     break;
		case TM_OP_DRAIN_BEGIN:   opn = "drain-begin";   break;
		case TM_OP_DRAIN_KEEP:    opn = "drain-keep";    break;
		case TM_OP_DRAIN_RELEASE: opn = "drain-release"; break;
		case TM_OP_DRAIN_END:     opn = "drain-end";     break;
		case TM_OP_SNAPSHOT_REC:  opn = "snapshot-rec";  break;
		case TM_OP_BITMAP_BUILT:  opn = "bitmap-built";  break;
		case TM_OP_BITMAP_HIT:    opn = "bitmap-hit";    break;
		case TM_OP_BITMAP_MISS:   opn = "bitmap-miss";   break;
		default:                  opn = "?";             break;
		}
		printf("  [%lu] %-14s sector=%lu gen=%lu count=%u\n",
		    (unsigned long)e->seq, opn,
		    (unsigned long)e->sector, (unsigned long)e->gen,
		    e->count);
	}
	return (sysctl_handle_long(oidp, &widx, 0, req));
}
SYSCTL_PROC(_kern_tessera, OID_AUTO, metatrace_dump,
    CTLTYPE_ULONG | CTLFLAG_RD | CTLFLAG_MPSAFE,
    NULL, 0, tessera_sysctl_metatrace_dump, "LU",
    "Read to print the meta-reserve trace ring to dmesg");

/* v2 slice-4 retention: cap on number of snapshot records retained
 * in the snapshots_tree. At every commit_sb, if the count exceeds
 * this horizon, the oldest (lowest-gen) record is btree_delete'd.
 *
 * Default chosen to keep meta-reserve growth bounded under typical
 * workloads while still giving useful time-machine depth. Users can
 * tune at runtime. Setting to 0 disables retention (snapshots
 * accumulate indefinitely — only fine for small/short-lived volumes
 * or testing). */
static int tessera_snapshot_retention = 16;
SYSCTL_INT(_kern_tessera, OID_AUTO, snapshot_retention,
    CTLFLAG_RW, &tessera_snapshot_retention, 0,
    "Cap on retained snapshot records (oldest dropped at next commit; 0 = unlimited)");
static unsigned long tessera_stat_snapshots_retired = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, snapshots_retired,
    CTLFLAG_RD, &tessera_stat_snapshots_retired, 0,
    "Cumulative snapshot records dropped by retention horizon");

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

/* Aggregation packs: small INLINE manifests batched together at
 * pending_manifests_drain time so we don't pay per-pack header/index
 * overhead per tiny file. ~1.5 KiB amortized overhead per manifest
 * vs ~16 KiB for single-blob packs. */
static unsigned long tessera_stat_aggregation_packs    = 0;
static unsigned long tessera_stat_aggregation_blobs    = 0;
static unsigned long tessera_stat_aggregation_dedups   = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, aggregation_packs, CTLFLAG_RD,
    &tessera_stat_aggregation_packs, 0,
    "Multi-blob aggregation packs emitted at drain");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, aggregation_blobs, CTLFLAG_RD,
    &tessera_stat_aggregation_blobs, 0,
    "Total blobs packed via aggregation (across all packs)");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, aggregation_dedups, CTLFLAG_RD,
    &tessera_stat_aggregation_dedups, 0,
    "Per-blob dedups skipped before aggregation (CAS-cache hits)");

/* Aggregation pack tunables. Cap each pack so individual writes
 * stay manageable and so a single pack's loss doesn't take out a
 * huge cluster of files. */
static unsigned long tessera_aggregation_max_blobs = 64;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, aggregation_max_blobs, CTLFLAG_RW,
    &tessera_aggregation_max_blobs, 0,
    "Max blobs per aggregation pack");
static unsigned long tessera_aggregation_max_bytes = 256u * 1024u;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, aggregation_max_bytes, CTLFLAG_RW,
    &tessera_aggregation_max_bytes, 0,
    "Max body bytes (sum of blob lengths) per aggregation pack");
/* Threshold: only batch if a manifest body is below this. Larger
 * manifests get their own single-blob pack like before — they don't
 * benefit much from amortizing pack overhead. */
static unsigned long tessera_aggregation_blob_max = 16u * 1024u;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, aggregation_blob_max, CTLFLAG_RW,
    &tessera_aggregation_blob_max, 0,
    "Max blob size eligible for aggregation (larger goes to a single-blob pack)");

/* v2 step-3 chunked-write observability — per-write-path counters
 * so future perf work can tell whether a workload is hitting the fast
 * paths or thrashing the slow rebuild path. RD-only; cumulative
 * across all mounts on this kmod load. */
static unsigned long tessera_stat_vop_write_inline    = 0;
static unsigned long tessera_stat_vop_write_chunked   = 0;
static unsigned long tessera_stat_chunk_dedup_skip    = 0;
static unsigned long tessera_stat_chunk_zero_hole     = 0;
static unsigned long tessera_stat_chunk_tree_publish  = 0;
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
SYSCTL_ULONG(_kern_tessera, OID_AUTO, chunk_tree_publish,
    CTLFLAG_RD, &tessera_stat_chunk_tree_publish, 0,
    "CHUNK_TREE outer-manifest publishes (write-side promotion)");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, append_fast_ok,
    CTLFLAG_RD, &tessera_stat_append_fast_ok, 0,
    "Append fast-path successes");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, append_fast_fallback,
    CTLFLAG_RD, &tessera_stat_append_fast_fallback, 0,
    "Append fast-path fallbacks to slow rewrite path");

/* fsync group commit (v2 polish): how many times a flush caller
 * waited on an already-in-flight commit instead of triggering a new
 * one. Each wait avoids one full commit_sb (5 sector writes). */
static unsigned long tessera_stat_fsync_group_wait = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, fsync_group_wait,
    CTLFLAG_RD, &tessera_stat_fsync_group_wait, 0,
    "fsync calls coalesced onto an already-in-flight commit");

/* v2 step-2b: total dirty inodes drained since module load.
 * High value vs sb_commits = effective batching. */
static unsigned long tessera_stat_dirty_drained = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, dirty_drained,
    CTLFLAG_RD, &tessera_stat_dirty_drained, 0,
    "Cumulative dirty inodes drained across all flushes");

static unsigned long tessera_stat_pending_drained = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, pending_drained,
    CTLFLAG_RD, &tessera_stat_pending_drained, 0,
    "Cumulative pending manifests drained across all flushes");

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
	struct g_consumer    *cp;   /* used by tessera_kbio_write to bypass
	                             * the buf cache for sectors that get
	                             * rewritten frequently — the buf cache
	                             * silently coalesces same-offset
	                             * bwrites on a g_vfs-opened devvp
	                             * (only the first reaches disk;
	                             * confirmed on virtio-blk + 9p both). */
	int                   flush_unsupported; /* set on first BIO_FLUSH that
	                                          * returns EOPNOTSUPP — disk
	                                          * advertises no volatile cache,
	                                          * so further barriers are no-ops
	                                          * (mirrors UFS's
	                                          * DISKFLAG_CANFLUSHCACHE gate). */
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

/* Per-sector BIO_FLUSH knob. Default 0 — commit_sb's two barrier
 * flushes (one before the SB write, one after) are sufficient for
 * crash recovery: the first ensures all prior pack/btree/journal-
 * record writes are durable BEFORE the SB sector that names them
 * is committed; the second ensures the SB is durable BEFORE
 * journal_checkpoint retires the records that would let replay
 * recover from a torn SB. Per-write FLUSH was a debugging
 * conservatism while bringing up Phase B.2; it costs ~2-2.6× on
 * medium-block writes (measured) and adds nothing the barrier
 * flushes don't already cover.
 *
 * Set to 1 if you want belt-and-suspenders or to bisect crash
 * issues that suspect the barrier model. */
static int tessera_flush_per_write = 0;
SYSCTL_INT(_kern_tessera, OID_AUTO, flush_per_write,
    CTLFLAG_RW, &tessera_flush_per_write, 0,
    "1 = BIO_FLUSH after every kbio_write (slower, paranoid); "
    "0 = rely on commit_sb's barrier flushes (default, faster)");

static int
tessera_kbio_write(void *ctx, uint64_t sector, const uint8_t *buf)
{
	struct tessera_kbio_ctx *k = ctx;
	struct buf *bp = getblk(k->devvp, sector * btodb(TESSERA_SECTOR_SIZE),
	    TESSERA_SECTOR_SIZE, 0, 0, 0);
	if (bp == NULL) return (-1);
	bzero(bp->b_data, TESSERA_SECTOR_SIZE);
	memcpy(bp->b_data, buf, TESSERA_SECTOR_SIZE);
	int rc = bwrite(bp);
	if (rc != 0) return rc;
	if (!tessera_flush_per_write) return 0;

	/* Issue an explicit BIO_FLUSH to force the underlying device
	 * (and, when running under qemu on macOS, the host backing
	 * file) to commit the data. macOS doesn't honor O_DIRECT, so
	 * even cache=none silently buffers writes in qemu's RAM until
	 * the guest sends a FLUSH command. Without this, sector writes
	 * are lost across qemu system_reset / quit even though bwrite
	 * returned success. virtio-blk must be configured with
	 * `config-wce=on` so VIRTIO_BLK_F_FLUSH is negotiated; otherwise
	 * the FLUSH is silently dropped by the host. */
	if (k->cp != NULL) {
		struct bio *flush = g_alloc_bio();
		flush->bio_cmd = BIO_FLUSH;
		flush->bio_done = NULL;
		flush->bio_offset = 0;
		flush->bio_length = 0;
		flush->bio_data = NULL;
		g_io_request(flush, k->cp);
		(void)biowait(flush, "tflush");
		g_destroy_bio(flush);
	}
	return 0;
}

/* Crash-durability barrier. Issue a single BIO_FLUSH on the
 * mount's GEOM consumer; on macOS+qemu this triggers an fsync()
 * of the backing host file. Callers use this at commit_sb's two
 * ordering points to keep durability semantics correct without
 * paying per-write FLUSH cost. */
static void
tessera_kbio_barrier(struct tessera_kbio_ctx *k)
{
	if (k == NULL || k->cp == NULL) return;
	if (k->flush_unsupported) return;
	struct bio *flush = g_alloc_bio();
	flush->bio_cmd = BIO_FLUSH;
	flush->bio_done = NULL;
	flush->bio_offset = 0;
	flush->bio_length = 0;
	flush->bio_data = NULL;
	g_io_request(flush, k->cp);
	int err = biowait(flush, "tbarr");
	g_destroy_bio(flush);
	/* Disk advertises no volatile write cache (e.g. virtio-blk
	 * without config-wce, ramdisk, write-cache-disabled HDD).
	 * Cache the result so we stop paying the failed-flush round-trip;
	 * mirrors UFS's DISKFLAG_CANFLUSHCACHE check. */
	if (err == EOPNOTSUPP) {
		k->flush_unsupported = 1;
		printf("tessera_fs: device does not support BIO_FLUSH, "
		    "barriers will be skipped (durability depends on host)\n");
	}
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
static void tessera_meta_pin_bitmap_rebuild(struct tessera_mount *tmp_);
static int tessera_fs_gc_data_zone(struct tessera_mount *tmp_);
struct tessera_pack_alloc_result;
static int tessera_fs_pack_alloc_and_write(struct tessera_mount *tmp_,
    const uint8_t *pack_bytes, uint64_t n_sectors,
    struct tessera_pack_alloc_result *out);
static int tessera_fs_pack_extents_resolve(struct tessera_mount *tmp_,
    const tessera_registry_entry_t *re,
    tessera_pack_extent_t **out_extents, uint32_t *out_count);
static int tessera_vop_write(struct vop_write_args *ap);
static int tessera_fs_read_full_content(struct tessera_mount *tmp_,
    const tessera_inode_record_t *ino,
    uint8_t **out_buf, size_t *out_size);
static int tessera_fs_replace_content(struct tessera_mount *tmp_,
    uint32_t inode_no, const uint8_t *new_bytes, size_t new_len);
static int tessera_fs_inode_unlink(struct tessera_mount *tmp_,
    uint32_t inode_no);
static int  tessera_fs_flush     (struct tessera_mount *tmp_);
static int  tessera_fs_dirty_content_write(struct tessera_mount *tmp_,
    uint32_t inode_no, uint64_t write_off, const uint8_t *new_bytes,
    size_t write_len, size_t final_size);
static int  tessera_fs_dirty_content_write_uio(struct tessera_mount *tmp_,
    uint32_t inode_no, uint64_t write_off, struct uio *uio,
    size_t final_size);
static int  tessera_fs_dirty_content_read(struct tessera_mount *tmp_,
    uint32_t inode_no, struct uio *uio);
static int  tessera_fs_dirty_content_drain_one(struct tessera_mount *tmp_,
    uint32_t inode_no);
static int  tessera_fs_dirty_content_drain_all(struct tessera_mount *tmp_);
static void tessera_fs_dirty_content_drop(struct tessera_mount *tmp_,
    uint32_t inode_no);
struct tessera_cas_cache;
static void tessera_cas_cache_init (struct tessera_cas_cache *c);
static void tessera_cas_cache_drain(struct tessera_cas_cache *c);
static void tessera_cas_loc_insert (struct tessera_cas_cache *c,
    const tessera_hash_t hash, const uint8_t pack_id[16],
    const tessera_pack_extent_t *exts, uint32_t nexts,
    uint64_t total_sectors);
/* Snapshot of a cache entry for the caller to consume after the
 * mtx is released — entries themselves can be evicted by other
 * threads, so we copy what we need under the lock. */
struct tessera_cas_loc_snap {
	uint8_t          pack_id[16];
	uint64_t         total_sectors;
	uint8_t          n_extents;
	tessera_pack_extent_t extents[4];
};
static int  tessera_cas_loc_lookup (struct tessera_cas_cache *c,
    const tessera_hash_t hash, struct tessera_cas_loc_snap *out);
static void tessera_cas_invalidate_pack(struct tessera_cas_cache *c,
    const uint8_t pack_id[16]);
/* Tier B: cache small hot blob *bytes*. Lookup returns a freshly
 * malloc'd copy (caller frees, just like fetch_blob's normal
 * return). Insert silently drops blobs > cas_small_blob_cap. */
static int  tessera_cas_byte_lookup(struct tessera_cas_cache *c,
    const tessera_hash_t hash, uint8_t **out_buf, uint32_t *out_len);
static void tessera_cas_byte_insert(struct tessera_cas_cache *c,
    const tessera_hash_t hash, const uint8_t *bytes, uint32_t length);
static void tessera_fs_mark_dirty(struct tessera_mount *tmp_);
static void tessera_fs_flush_task(void *ctx, int pending);
static void tessera_fs_repack_task(void *ctx, int pending);
static int  tessera_fs_repack_pass(struct tessera_mount *tmp_,
                                   uint32_t max_packs, uint32_t max_time_ms,
                                   uint32_t *out_repacked);
static int  tessera_fs_count_multi_extent(struct tessera_mount *tmp_,
                                          uint32_t *out_count);

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
static int tessera_fs_append_chunk_tree(struct tessera_mount *tmp_,
    uint32_t inode_no, const uint8_t *append_bytes, size_t append_len,
    uint32_t cs);
static void encode_inode_key(uint32_t inode_no, uint8_t out[4]);

/* v2 step-2b: dirty inode entry — element of the per-mount hash. */
struct tessera_dirty_inode {
	LIST_ENTRY(tessera_dirty_inode) link;
	uint32_t                inode_no;
	int                     tombstone;   /* 1 = drain calls btree_delete */
	tessera_inode_record_t  rec;
};

/* Per-inode dirty content buffer.
 *
 * Without this, every vop_write does:
 *   read full content → memcpy new bytes → publish new manifest pack
 * — quadratic in number of writes for a file growing under the
 * INLINE threshold (256 KiB), and per-call expensive even for the
 * chunked-append fast path. With this buffer:
 *   vop_write → memcpy into buffer; mark dirty; return
 *   tessera_fs_flush → publish each dirty buffer once
 *
 * Closes the gap with conventional filesystems on small-write
 * workloads (4 KB dd: ~1 MB/s before → comparable to UFS after).
 *
 * Memory cap: tessera_dirty_content_cap (default 64 MiB) limits
 * the total bytes held across all inodes; a write that would push
 * past the cap forces a flush of the largest dirty buffer first.
 *
 * Locking: same flush_mtx as dirty_inodes / pending_manifests. */
struct tessera_dirty_content {
	LIST_ENTRY(tessera_dirty_content) link;
	uint32_t  inode_no;
	uint8_t  *bytes;        /* malloc'd, length = capacity */
	size_t    size;         /* logical file size (≤ capacity) */
	size_t    capacity;     /* allocated buffer size */
	int       dirty;        /* 1 = differs from on-disk manifest */
};

/* CAS read cache — see docs/cas_cache_plan.md.
 *
 * tessera_fs_fetch_blob() previously linearly scanned pack_registry
 * to find which pack contains a given blob hash, then bread the pack
 * and parsed it. O(N_packs) per fetch — the dominant cost in the
 * 4KB×256-fsync benchmark.
 *
 * Tier A — location entries: blob_hash → (pack_id, extents, offset
 * inside pack, length). Lookup is O(1); on hit we skip the linear
 * scan, bread the cached extents directly, and parse one pack.
 *
 * Tier B (stage 5) — bytes entries: blob_hash → bytes copy. For
 * small hot blobs (manifests, dirents). On hit we skip even the
 * bread/parse and return immediately.
 *
 * Both tiers: LRU eviction, single mutex, M_NOWAIT inserts so the
 * cache never blocks fetch_blob's slow path under memory pressure.
 */
struct tessera_cas_loc_entry {
	LIST_ENTRY(tessera_cas_loc_entry) hash_link;
	TAILQ_ENTRY(tessera_cas_loc_entry) lru_link;
	tessera_hash_t   hash;            /* the blob hash we cache by */
	uint8_t          pack_id[16];     /* registry key — for invalidation by pack */
	uint64_t         total_sectors;   /* sum of extents[i].length_sectors */
	uint8_t          n_extents;       /* 1..4 inline; 0xFF = fall back to resolver */
	tessera_pack_extent_t extents[4];
};

struct tessera_cas_byte_entry {
	LIST_ENTRY(tessera_cas_byte_entry) hash_link;
	TAILQ_ENTRY(tessera_cas_byte_entry) lru_link;
	tessera_hash_t   hash;
	uint32_t         length;
	uint8_t         *bytes;          /* malloc'd, length bytes */
};

#define TESSERA_CAS_LOC_BUCKETS    1024u
#define TESSERA_CAS_BYTE_BUCKETS    256u

TAILQ_HEAD(tessera_cas_loc_lru,  tessera_cas_loc_entry);
TAILQ_HEAD(tessera_cas_byte_lru, tessera_cas_byte_entry);

struct tessera_cas_cache {
	struct mtx      mtx;
	int             mtx_init;
	/* Tier A — location */
	LIST_HEAD(, tessera_cas_loc_entry)   loc_buckets[TESSERA_CAS_LOC_BUCKETS];
	struct tessera_cas_loc_lru           loc_lru;
	size_t          loc_count;
	/* Tier B — bytes (stage 5) */
	LIST_HEAD(, tessera_cas_byte_entry)  byte_buckets[TESSERA_CAS_BYTE_BUCKETS];
	struct tessera_cas_byte_lru          byte_lru;
	size_t          byte_bytes;
};

/* v2.6 Phase B.2 — pending inode-body record awaiting journal
 * commit. Cloned by inode_put / inode_delete onto the
 * journal_pending_inodes list under flush_mtx; drained by the
 * group-commit callout into INODE_WRITE journal records. */
struct tessera_pending_inode {
	LIST_ENTRY(tessera_pending_inode) link;
	uint32_t                inode_no;
	int                     tombstone;
	tessera_inode_record_t  rec;
};

/* v2 step-2b: pending-manifest cache entry. Manifest bytes that have
 * had their hash computed and returned to the caller, but haven't
 * been written to a pack on disk yet. fetch_blob consults this cache
 * before scanning pack_registry, so subsequent reads of the manifest
 * during the same flush window come straight from RAM.
 *
 * Only manifests are cached, not chunk blobs — keeps the cache
 * size bounded by manifest churn rather than file content size.
 * The publish_chunked path still writes chunk bytes to disk
 * synchronously; only the CHUNK_LIST manifest itself is deferred. */
/* List of owners — every inode whose manifest_hash currently points
 * at this pending manifest. Multiple inodes can share the same hash
 * via CAS dedup (e.g. four files all containing "x\n"); supersession
 * must only delete the entry when the LAST owner has moved on. */
struct tessera_pending_owner {
	LIST_ENTRY(tessera_pending_owner) link;
	uint32_t inode_no;
};

struct tessera_pending_manifest {
	LIST_ENTRY(tessera_pending_manifest) link;
	tessera_hash_t  hash;
	uint8_t        *bytes;
	uint32_t        len;
	/* Owners that currently reference this manifest hash. Empty list
	 * + insertion via the "untagged" path (owner_inode_no=0 at put
	 * time) means "anyone may reference this; never supersede". */
	LIST_HEAD(, tessera_pending_owner) owners;
};

/* v2.6 dirent log entry. Variable-length: name bytes follow the
 * struct (flexible array). seq is monotonic — most-recent entries
 * have the highest seq, used for read-side ordering when multiple
 * ops touch the same name (ADD-then-REMOVE-then-ADD etc.). */
struct tessera_dirent_log_entry {
	LIST_ENTRY(tessera_dirent_log_entry) link;
	uint32_t parent_inode_no;
	uint32_t inode_no;
	uint64_t name_hash;
	uint64_t seq;
	uint8_t  op;             /* 0 = ADD, 1 = REMOVE */
	uint16_t name_len;
	char     name[];
};
static int tessera_fs_inode_get(struct tessera_mount *tmp_,
                                uint32_t inode_no,
                                tessera_inode_record_t *out);
/* Drop-in wrappers that match the existing tessera_btree_{get,put,
 * delete}(tmp_->inode_tree, ...) signatures — each decodes inode_no
 * from the 4-byte key, then routes through the dirty_inodes cache.
 * Lets us batch-refactor with replace_all without touching the
 * surrounding new_root dance. The put/delete wrappers also write
 * the current sb.inode_root back into *out_root so existing
 * `tmp_->sb.inode_root = new_root;` lines remain harmless no-ops. */
static int tessera_fs_inode_get_byk(struct tessera_mount *tmp_,
                                    const uint8_t key[4],
                                    tessera_inode_record_t *out);
static int tessera_fs_inode_get_at_gen(struct tessera_mount *tmp_,
                                       uint32_t inode_no,
                                       uint64_t snapshot_gen,
                                       tessera_inode_record_t *out);
static int tessera_fs_inode_put_byk(struct tessera_mount *tmp_,
                                    const uint8_t key[4],
                                    const tessera_inode_record_t *rec,
                                    uint64_t *out_root);
static int tessera_fs_inode_delete_byk(struct tessera_mount *tmp_,
                                       const uint8_t key[4],
                                       uint64_t *out_root);
static int tessera_fs_inode_put(struct tessera_mount *tmp_,
                                uint32_t inode_no,
                                const tessera_inode_record_t *rec);
static int tessera_fs_inode_delete(struct tessera_mount *tmp_,
                                   uint32_t inode_no);
static int tessera_fs_dirty_inodes_drain(struct tessera_mount *tmp_);

/* v2 step-2b: pending-manifest cache helpers. */
static int tessera_fs_pending_manifest_put(struct tessera_mount *tmp_,
                                           const tessera_hash_t hash,
                                           const uint8_t *bytes,
                                           uint32_t len,
                                           uint32_t owner_inode_no);
static int tessera_fs_pending_manifest_lookup(struct tessera_mount *tmp_,
                                              const tessera_hash_t hash,
                                              uint8_t **out_bytes,
                                              uint32_t *out_len);
static int tessera_fs_pending_manifests_drain(struct tessera_mount *tmp_);
/* The actual disk-publish path used both by direct callers AND by
 * the drain. Splits out the bytes-to-pack-on-disk work from the
 * cache-or-disk decision. */
static int tessera_fs_publish_manifest_to_disk(struct tessera_mount *tmp_,
                                               const uint8_t *manifest_bytes,
                                               size_t mlen,
                                               const tessera_hash_t hash);
struct tessera_aggr_entry {
	const uint8_t  *bytes;
	uint32_t        len;
	tessera_hash_t  hash;
};
static int tessera_fs_publish_manifests_batch(struct tessera_mount *tmp_,
    const struct tessera_aggr_entry *entries, uint32_t n);
static int tessera_fs_publish_manifest_owned(struct tessera_mount *tmp_,
                                             const uint8_t *manifest_bytes,
                                             size_t mlen,
                                             tessera_hash_t out_hash,
                                             uint32_t owner_inode_no);
/* Threshold for forcing a drain: cap RAM held in pending. ~1 MiB
 * default. Past this, mark_dirty triggers a flush. */
/* Bump from 1 MiB → 16 MiB. Sustained dirent mutations (stress2's
 * link/rename testcases, K=256 2L fast path) publish ~3 KiB of
 * bucket+outer bytes per op. At 1 MiB the cache fills every ~333 ops
 * and drives a synchronous commit_sb whose I/O dominates per-op cost.
 * 16 MiB lets a typical batch run to completion before any
 * meta-reserve drain pressure kicks in; commit cadence then ends up
 * driven by fsync / mark_dirty / 5s timer rather than cache size. */
#define TESSERA_PENDING_MANIFEST_BYTES_MAX  (16u * 1024u * 1024u)

/* v2 multi-level directory helpers. */
typedef int (*tessera_dirent_cb_t)(void *ctx, uint64_t child_inode,
                                   const char *name, uint16_t name_len);
static int tessera_fs_dir_walk(struct tessera_mount *tmp_,
                               const tessera_hash_t dir_manifest_hash,
                               tessera_dirent_cb_t cb, void *ctx);
static int tessera_fs_dirent_rewrite(struct tessera_mount *tmp_,
                                     uint32_t parent_inode_no,
                                     int op, uint64_t verify_inode,
                                     uint64_t add_inode,
                                     const char *name, size_t namelen);
/* CHUNK_LIST / CHUNK_TREE recursive reader (v2 step-3c). */
static int tessera_fs_read_into_uio(struct tessera_mount *tmp_,
                                    tessera_manifest_parser_t *p,
                                    struct uio *uio);
static int tessera_fs_publish_directory(struct tessera_mount *tmp_,
                                         uint32_t owner_inode_no,
                                         const uint8_t *flat_mft,
                                         size_t flat_mlen,
                                         tessera_hash_t out_hash);
/* DIRECTORY_BTREE helpers — forward decls so vop_lookup / vop_readdir
 * can call into them before the implementation block. */
static int tessera_fs_dir_btree_lookup(struct tessera_mount *,
    const tessera_hash_t root_hash, const char *name, uint16_t namelen,
    uint64_t *out_inode);
static int tessera_fs_dir_btree_walk(struct tessera_mount *,
    const tessera_hash_t root_hash, tessera_dirent_cb_t cb, void *ctx);
static int tessera_fs_dir_btree_decode(uint8_t *blob, uint32_t blob_len,
    int *out_leaf, const uint8_t **out_body, size_t *out_body_len,
    uint32_t *out_count);
static int tessera_fs_dir_btree_insert(struct tessera_mount *tmp_,
    const tessera_hash_t root_hash, int root_is_empty,
    const char *name, uint16_t namelen, uint64_t inode_no,
    tessera_hash_t out_new_root);
static int tessera_fs_dir_btree_remove(struct tessera_mount *tmp_,
    const tessera_hash_t root_hash, const char *name, uint16_t namelen,
    uint64_t verify_inode, int *out_dropped, tessera_hash_t out_new_root);
static int tessera_fs_dir_btree_migrate(struct tessera_mount *tmp_,
    const tessera_hash_t old_dir_hash, tessera_hash_t out_new_root,
    int *out_empty);

/* v2.6 dirent log helpers — forward decls so vop_lookup /
 * vop_readdir / tessera_fs_flush / mark_dirty can reach them
 * before the implementation block. The on/off knob and
 * cap-trigger threshold need to be declared here too because
 * mark_dirty (also above the impl) reads them. */
#define TESSERA_DIRENT_LOG_MISS  (-1)
static int tessera_dirent_log_enable_default;
static int tessera_dirent_log_threshold;
static int tessera_fs_dirent_log_append(struct tessera_mount *tmp_,
    uint32_t parent_inode_no, int op,
    const char *name, uint16_t namelen, uint64_t inode_no);
static int tessera_fs_dirent_log_lookup(struct tessera_mount *tmp_,
    uint32_t parent_inode_no, const char *name, uint16_t namelen,
    int *out_op, uint64_t *out_inode);
static int tessera_fs_dirent_log_checkpoint_parent(
    struct tessera_mount *tmp_, uint32_t parent_inode_no);
static int tessera_fs_dirent_log_checkpoint_all(
    struct tessera_mount *tmp_);

/* v2.6 Phase B.2 — journal-resident dirent records. Forward decls
 * so mountfs / tessera_fs_flush / tessera_replay_handler reach
 * them before the implementation block. The on/off + interval
 * sysctls also need declaration here because mountfs (also above
 * the impl) reads them when arming the callout. */
static int tessera_journal_log_enable_default;
static int tessera_journal_log_interval_ms;
static unsigned long tessera_stat_journal_log_replays = 0;
static int tessera_fs_journal_log_drain(struct tessera_mount *tmp_);
static void tessera_fs_journal_log_callout(void *ctx);
static void tessera_fs_journal_log_task(void *ctx, int pending);
static int tessera_replay_dirent_record(struct tessera_mount *tmp_,
    const tessera_record_header_t *hdr, const uint8_t *body);
static struct tessera_dirent_log_entry *tessera_dirent_log_entry_clone(
    const struct tessera_dirent_log_entry *src);
/* Promotion threshold: when a flat-DIRECTORY manifest body exceeds
 * this size, publish_directory splits entries into hash buckets and
 * emits DIRECTORY_2L instead. ~4 KiB matches the v2 design; small
 * enough that mutation cost stays bounded, large enough that typical
 * small dirs stay flat. */
#define TESSERA_DIR_PROMOTE_THRESHOLD  (4u * 1024u)
/* Bucket count when promoting. K=16 keeps each bucket ~100 entries
 * for dir sizes around the promote threshold. Larger dirs (>1k
 * entries) will eventually want 3 levels — deferred. */
/* K=256 buckets (top 8 bits of dir_name_hash select). With K=16
 * (top 4 bits) per-bucket walks were ~N/16 entries; under stress2's
 * 100×size-loop link/rename testcases that's 5–15 ms/op for parents
 * over 1k entries. K=256 cuts the bucket walk to N/256, dropping
 * per-op into the µs range up to N≈64k.
 *
 * Format-compatible with K=16 volumes: lookup binary-searches outer
 * by first_name_hash, doesn't care which K split a bucket. Old
 * 16-bucket manifests continue to work; new mutations gradually
 * republish into the 256-slot scheme. */
#define TESSERA_DIR_BUCKET_COUNT       256u

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

	/* v2 slice-4: snapshot meta-reserve pin bitmap. One bit per
	 * sector in [meta_reserve_start, meta_reserve_start + meta_reserve_length).
	 * Bit SET ⇔ that sector is referenced by some retained snapshot's
	 * btree. Built at mount time by walking every retained snapshot's
	 * tree node sectors (same logic that the orphan-reclaimer already
	 * used at lines 939-1045 — now persisted on the mount struct).
	 *
	 * commit_sb's drain filters meta_pending against this bitmap:
	 * sectors with their bit SET stay pinned (would corrupt a forensic
	 * mount of the older gen if recycled); sectors with their bit
	 * CLEAR are released to meta_free for reuse.
	 *
	 * NOT updated mid-session — once a sector is in the bitmap, it
	 * stays there until unmount. Retention (clearing bits when an old
	 * snapshot is dropped) is the next step, currently not implemented;
	 * with no retention the bitmap monotonically grows but is bounded
	 * by meta_reserve_length bits = ~32 KB for a 64 MiB reserve. */
	uint8_t                  *meta_pin_bitmap;
	size_t                    meta_pin_bitmap_bytes;

	/* v2-step-2a: deferred-commit state. See sysctl block above. */
	int                       sb_dirty;
	int                       flush_co_init;     /* callout initialised */
	int                       flush_unmounting;  /* don't rearm callout */
	struct callout            flush_co;
	struct task               flush_task;

	/* v2 repack engine slice 3: background trigger.
	 * multi_extent_pack_count tracks how many MULTI_EXTENT-flagged
	 * packs live in the registry. Maintained as a delta:
	 * pack_alloc_and_write's multi path increments on success,
	 * tessera_fs_repack_one_pack decrements on success. Initial
	 * value is set at mount time by walking the registry once.
	 *
	 * mark_dirty arms `repack_task` (background taskqueue) when the
	 * count exceeds tessera_repack_threshold. The handler runs a
	 * bounded repack pass and re-arms if more work remains. */
	uint32_t                  multi_extent_pack_count;
	struct task               repack_task;
	int                       repack_task_init;

	/* v2 polish: fsync group-commit. Multiple processes calling
	 * fsync (or vop_fsync via the deferred-flush callout) coalesce
	 * onto a single in-flight commit_sb instead of each triggering
	 * its own. flush_mtx protects flush_in_progress + sb_dirty
	 * coordination; latecomers msleep on &flush_in_progress and
	 * wakeup when the active commit clears it. */
	struct mtx                flush_mtx;
	int                       flush_mtx_init;
	int                       flush_in_progress;

	/* v2-step-2b: dirty-inode write-back cache. Mutations stage
	 * the new inode record here instead of immediately btree_put-
	 * ting it; a flush drains everything via the normal btree_put
	 * + commit_sb path. Read sites consult dirty_inodes first,
	 * fall back to btree_get. Tombstones mark inodes that were
	 * unlinked while dirty (drain calls btree_delete). All accesses
	 * take flush_mtx — same lock that protects the commit-coordi-
	 * nation flag, so a flush sees a coherent snapshot. */
#define TESSERA_DIRTY_INODE_BUCKETS  128u
	LIST_HEAD(, tessera_dirty_inode) dirty_inodes[TESSERA_DIRTY_INODE_BUCKETS];
	uint32_t                  dirty_count;
	int                       dirty_init;

	/* Per-inode dirty content buffers (write coalescing). See
	 * struct tessera_dirty_content. */
#define TESSERA_DIRTY_CONTENT_BUCKETS  128u
	LIST_HEAD(, tessera_dirty_content) dirty_content[TESSERA_DIRTY_CONTENT_BUCKETS];
	size_t                    dirty_content_bytes; /* sum of size across all */

	/* CAS read cache (see struct tessera_cas_cache + cas_cache_plan.md). */
	struct tessera_cas_cache cas_cache;

	/* v2 step-2b: pending-manifest cache. Same flush_mtx covers
	 * both. */
#define TESSERA_PENDING_MANIFEST_BUCKETS  256u
	LIST_HEAD(, tessera_pending_manifest)
	    pending_manifests[TESSERA_PENDING_MANIFEST_BUCKETS];
	uint32_t                  pending_manifest_count;
	uint64_t                  pending_manifest_bytes;

	/* v2.6 dirent log: in-memory delta log of pending dirent
	 * mutations. Each dirent_rewrite call appends a record here
	 * instead of immediately rewriting the parent's BTREE.
	 * Checkpoint (called from flush + readdir + cap-trigger) does
	 * one bulk merge of the log into each affected parent's
	 * BTREE, producing a single new root per parent regardless of
	 * how many ops were buffered. Per-op cost drops to "list
	 * append"; per-checkpoint cost is one BTREE rebuild per dirty
	 * parent.
	 *
	 * Lookups consult the log first (most-recent op for a name
	 * wins; ADD → return inode, REMOVE → ENOENT, no entry → fall
	 * through to BTREE).
	 *
	 * Crash safety: log is RAM-only. fsync triggers checkpoint
	 * → BTREE update → commit_sb. Same durability as today's
	 * dirty_inodes cache. */
#define TESSERA_DIRENT_LOG_BUCKETS  64u
	LIST_HEAD(, tessera_dirent_log_entry)
	    dirent_log[TESSERA_DIRENT_LOG_BUCKETS];
	uint32_t                  dirent_log_count;
	uint64_t                  dirent_log_seq;   /* monotonic */

	/* v2.6 Phase B.2 — journal-resident dirent records.
	 * Each log_append clones an entry onto journal_pending; a
	 * group-commit callout (journal_log_co) drains the queue into
	 * the journal as one tx every TESSERA_JOURNAL_LOG_INTERVAL_MS.
	 * After a successful drain the entries are freed; the durable
	 * journal records get superseded when commit_sb's
	 * journal_checkpoint advances the log past them.
	 *
	 * On crash + remount, replay re-creates entries in the
	 * in-memory log; the first post-mount flush applies them. */
	LIST_HEAD(, tessera_dirent_log_entry) journal_pending;
	uint32_t                  journal_pending_count;
	/* Inode-body counterpart: enqueued by tessera_fs_inode_put /
	 * tessera_fs_inode_delete and drained alongside the dirent
	 * pending list. Without these records, replayed dirents
	 * reference inode_nos whose body never made it to disk. */
	LIST_HEAD(, tessera_pending_inode) journal_pending_inodes;
	uint32_t                  journal_pending_inode_count;
	struct callout            journal_log_co;
	struct task               journal_log_task;
	int                       journal_log_co_init;
	/* Set during the mount-time tessera_journal_replay walk so the
	 * inode_put / inode_delete / dirent_log_append paths SKIP the
	 * journal-pending enqueue. Without this, replay re-journals
	 * every record it just read; the next commit_sb's
	 * journal_checkpoint cleans them up so it's net-benign, but
	 * pure waste on every mount. */
	int                       in_replay;

	/* v2 snapshots slice 2: read-only historical mount via
	 * `tessera.gen=N`. When 1, mountfs overrode sb roots from a
	 * snapshot record; mutation paths are blocked by MNT_RDONLY,
	 * mount-time GC + meta-recycler are skipped, commit_sb won't
	 * fire (sb_dirty never gets set since mark_dirty isn't called). */
	int                       readonly_snapshot;
	uint64_t                  snapshot_gen;

	/* v2 step-3c prereq: per-mount chunk-size override. When 0 (the
	 * default), tessera_chunk_size_for() returns the auto-tier size
	 * keyed off file_size (64 KiB / 1 MiB / 4 MiB). When non-zero,
	 * every chunked write on this mount uses this size verbatim.
	 *
	 * Set via `mount -o tessera.chunk_size=<bytes>`. Validated at
	 * parse time to be a power-of-two in [4096, 4194304]. The
	 * intended use is VM-image directories that benefit from 4 KiB
	 * chunks (matching guest fs block size for fine-grained dedup);
	 * CHUNK_TREE write-side promotion will lift the resulting
	 * manifest-size cost via log(N) amplification. */
	uint32_t                  chunk_size_override;

	/* Quota (tessera-quotas.md). v1 = a single whole-FS domain,
	 * activated by `mount -o tessera.quota_bytes=N`. quota_active gates
	 * every check; quota_dom carries the running used_bytes against
	 * limit_bytes (logical bytes, §3.2). used_bytes starts at 0 — correct
	 * for a freshly-mkfs'd volume; an accurate mount-time used for a
	 * pre-populated FS, per-directory multi-domain, and the inode
	 * quota_domain inheritance are the next increments. Guarded by the
	 * vnode lock on the write/truncate paths (single-domain, no separate
	 * lock needed yet). */
	int                       quota_active;
	tessera_quota_domain_t    quota_dom;
};

/* Allocate the next inode_no atomically. The bare
 * `new = sb.next_inode_no; sb.next_inode_no = new+1;` pattern was
 * latently racy — three callers (vop_create, vop_mkdir, vop_symlink)
 * shared the field. VFS happens to serialize same-parent creates via
 * the parent's exclusive vnode lock so the bug never fires today,
 * but it'd start losing inode numbers (collisions, then panic) the
 * moment that invariant changes. flush_mtx is the obvious shared
 * mutex; we already use it for sb.inode_root coordination. */
static inline uint32_t
tessera_fs_alloc_inode_no(struct tessera_mount *tmp_)
{
	mtx_lock(&tmp_->flush_mtx);
	uint32_t n = (uint32_t)tmp_->sb.next_inode_no;
	if (n < TESSERA_INODE_FIRST_USER) n = TESSERA_INODE_FIRST_USER;
	tmp_->sb.next_inode_no = n + 1;
	mtx_unlock(&tmp_->flush_mtx);
	return n;
}

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
		tessera_metatrace(TM_OP_ALLOC_REUSE, *out_sector,
		    tmp_->sb.generation, tmp_->meta_free_count);
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
	tessera_metatrace(TM_OP_ALLOC_BUMP, *out_sector,
	    tmp_->sb.generation, (uint32_t)used + 1);
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
	tessera_metatrace(TM_OP_FREE_PUSH, s, tmp_->sb.generation,
	    tmp_->meta_pending_count);
	return (0);
}

#define VFSTOTESSERA(mp) ((struct tessera_mount *)((mp)->mnt_data))

/* ── per-vnode private ──────────────────────────────────────── */

/*
 * v2 snapshots slice 3: magic dir at /.tessera/snapshots/<N>/.
 *
 * Most vnodes are TESSERA_NODE_REGULAR — backed by an actual inode
 * record in the inode_tree. The slice-3 magic-dir hierarchy uses
 * synthesized vnodes:
 *
 *   /.tessera                       — TESSERA_NODE_MAGIC_TESSERA
 *   /.tessera/snapshots             — TESSERA_NODE_MAGIC_SNAPSHOTS
 *   /.tessera/snapshots/<gen>       — TESSERA_NODE_REGULAR, snapshot_gen=N,
 *                                     inode_no=1 (snapshot's root inode)
 *   /.tessera/snapshots/<gen>/...   — TESSERA_NODE_REGULAR, snapshot_gen=N,
 *                                     inode_no=child looked up via the
 *                                     snapshot's inode_tree
 *
 * REGULAR with snapshot_gen != 0 is read-only — every mutation vop
 * checks for it and returns EROFS. Reads use the snapshot's
 * inode_tree (opened on demand from `srec.inode_root`); the live
 * pack_registry is fine for blob fetches because GC unionization
 * (slice 1) keeps the snapshot-referenced packs registered there.
 */
enum tessera_node_kind {
	TESSERA_NODE_REGULAR        = 0,
	TESSERA_NODE_MAGIC_TESSERA  = 1,
	TESSERA_NODE_MAGIC_SNAPSHOTS= 2,
};

/* Synthetic d_fileno values for magic-dir vnodes. Real tessera
 * inodes are uint32_t and allocated sequentially from low values,
 * so the high-bit range is safe to reserve. d_fileno=0 must be
 * avoided — getdirentries(2) treats it as a tombstone and ls skips
 * the entry. */
#define TESSERA_MAGIC_INO_TESSERA    0xFFFFFFF0u
#define TESSERA_MAGIC_INO_SNAPSHOTS  0xFFFFFFF1u

struct tessera_node {
	uint64_t inode_no;
	/* Parent inode_no, tracked at descent time so vop_lookup of ".."
	 * can return the right vnode. The root vnode loops back to
	 * itself per the standard FS convention. */
	uint64_t parent_inode_no;
	/* v2 slice-3: magic dir kind + per-vnode snapshot context. */
	uint8_t  kind;          /* enum tessera_node_kind */
	uint64_t snapshot_gen;  /* 0 = live; non-zero = read from gen N */
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
tessera_mountfs(struct vnode *devvp, struct mount *mp, uint64_t requested_gen,
    uint32_t chunk_size_override, uint64_t quota_bytes)
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
	tmp_->chunk_size_override = chunk_size_override;
	/* v1 whole-FS quota: one domain (id 1, rooted at the root inode 2),
	 * used starts 0. tmp_ was M_ZERO'd so quota_active defaults off. */
	if (quota_bytes > 0) {
		tessera_quota_domain_init(&tmp_->quota_dom, 1, 2, quota_bytes);
		tmp_->quota_active = 1;
		printf("tessera_fs: quota active, limit %ju bytes\n",
		    (uintmax_t)quota_bytes);
	}
	/* `active` aliases sb_a or sb_b — copy completed, free the
	 * heap-alloc'd staging copies. NULL them out so the fail_close
	 * cleanup at the bottom (which does `if (sb_a) free(sb_a)`) doesn't
	 * double-free if a later step (e.g. bogus tessera.gen=N) jumps to
	 * fail_close. Double-free of sector-sized blocks corrupts the
	 * malloc allocator in a way that manifests as a kernel spin on
	 * the next allocation — exactly the gen=999 hang signature. */
	free(sb_a, M_TESSERA); sb_a = NULL;
	free(sb_b, M_TESSERA); sb_b = NULL;

	/* Wire the kmod block_io shim and open the inode tree against it.
	 * If the tree open fails (e.g. corrupted root sector) we still
	 * mount — the synthesized root vnode lets `df` / `umount` work
	 * for diagnostic purposes. */
	tmp_->bio_ctx.devvp = devvp;
	tmp_->bio_ctx.cred  = curthread->td_ucred;
	tmp_->bio_ctx.mount = tmp_;
	tmp_->bio_ctx.cp    = cp;
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
	TASK_INIT(&tmp_->repack_task, 0, tessera_fs_repack_task, tmp_);
	tmp_->repack_task_init = 1;
	tmp_->multi_extent_pack_count = 0;
	mtx_init(&tmp_->flush_mtx, "tess_flush", NULL, MTX_DEF);
	tmp_->flush_mtx_init = 1;
	tmp_->flush_co_init = 1;
	for (uint32_t _b = 0; _b < TESSERA_DIRTY_INODE_BUCKETS; _b++)
		LIST_INIT(&tmp_->dirty_inodes[_b]);
	for (uint32_t _b = 0; _b < TESSERA_PENDING_MANIFEST_BUCKETS; _b++)
		LIST_INIT(&tmp_->pending_manifests[_b]);
	for (uint32_t _b = 0; _b < TESSERA_DIRENT_LOG_BUCKETS; _b++)
		LIST_INIT(&tmp_->dirent_log[_b]);
	for (uint32_t _b = 0; _b < TESSERA_DIRTY_CONTENT_BUCKETS; _b++)
		LIST_INIT(&tmp_->dirty_content[_b]);
	tmp_->dirty_content_bytes = 0;
	tessera_cas_cache_init(&tmp_->cas_cache);
	LIST_INIT(&tmp_->journal_pending);
	tmp_->journal_pending_count = 0;
	LIST_INIT(&tmp_->journal_pending_inodes);
	tmp_->journal_pending_inode_count = 0;
	callout_init(&tmp_->journal_log_co, 1);
	TASK_INIT(&tmp_->journal_log_task, 0, tessera_fs_journal_log_task,
	    tmp_);
	tmp_->journal_log_co_init = 1;
	tmp_->dirty_init = 1;

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
		printf("tessera_fs: journal_open OK at sector %lu len %lu; "
		    "starting replay\n",
		    (unsigned long)tmp_->sb.journal_start,
		    (unsigned long)tmp_->sb.journal_length);
		tmp_->in_replay = 1;
		(void)tessera_journal_replay(tmp_->journal,
		    tessera_replay_handler, &rctx);
		tmp_->in_replay = 0;
		printf("tessera_fs: replay applied %d ROOT_UPDATE record(s); "
		    "%lu DIR records re-applied\n",
		    rctx.applied,
		    tessera_stat_journal_log_replays);
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
		/* v2 slice-4: this bitmap is also consulted at every
		 * commit_sb drain to filter snapshot-pinned sectors out of
		 * meta_pending → meta_free reuse. So instead of allocating
		 * locally and free()ing at scope exit, persist on the mount. */
		uint8_t *bitmap = malloc(bitmap_bytes, M_TESSERA,
		    M_WAITOK | M_ZERO);
		tmp_->meta_pin_bitmap       = bitmap;
		tmp_->meta_pin_bitmap_bytes = bitmap_bytes;
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
		/* Bitmap persists on tmp_->meta_pin_bitmap — used by
		 * commit_sb drain. Freed at unmount. */
		tessera_metatrace(TM_OP_BITMAP_BUILT, 0,
		    tmp_->sb.generation,
		    (uint32_t)tmp_->meta_pin_bitmap_bytes);
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
	/* B1 test hook: stash the most-recently-mounted tessera in a
	 * file-static so the kern.tessera.repack_one sysctl can reach
	 * it without walking the mount list. Single-tessera per host is
	 * the test reality; multi-mount support if needed later iterates
	 * via vfs_busyfs. */
	tessera_singleton_mount = tmp_;

	/* v2.6 Phase B.2: arm the journal-log group-commit callout once
	 * mount is ready. Replay above may have populated the in-memory
	 * log + journal_pending; the first callout drains them so even
	 * if the user never fsyncs, those records become durable on the
	 * journal again (idempotent). */
	{
		int t_ms = tessera_journal_log_interval_ms;
		if (t_ms < 10) t_ms = 10;
		if (tmp_->journal_log_co_init && tmp_->journal != NULL)
			callout_reset(&tmp_->journal_log_co,
			    (hz * t_ms) / 1000,
			    tessera_fs_journal_log_callout, tmp_);
	}

	/* C2/C3 — seed multi_extent_pack_count from the registry, then
	 * if it's above the severe threshold, run a bounded synchronous
	 * repack pass before mount returns. Caps at 100 packs / 1 s by
	 * default; extra work happens in the background after first
	 * writes. Skipped on read-only forensic mounts. */
	if (!tmp_->readonly_snapshot) {
		uint32_t mc = 0;
		(void)tessera_fs_count_multi_extent(tmp_, &mc);
		tmp_->multi_extent_pack_count = mc;
		if (mc > 0)
			printf("tessera_fs: %u MULTI_EXTENT pack(s) at mount\n", mc);
		if ((int)mc > tessera_repack_severe_threshold) {
			uint32_t budget_packs =
			    (uint32_t)tessera_repack_mount_max_packs;
			uint32_t budget_ms =
			    (uint32_t)tessera_repack_mount_max_time_ms;
			if (budget_packs == 0) budget_packs = 100;
			if (budget_ms == 0) budget_ms = 1000;
			uint32_t repacked = 0;
			printf("tessera_fs: mount-time repack — %u packs over "
			    "threshold %d, running bounded pass (%u/%u)\n",
			    mc, tessera_repack_severe_threshold,
			    budget_packs, budget_ms);
			(void)tessera_fs_repack_pass(tmp_, budget_packs,
			    budget_ms, &repacked);
		}
	}
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

	/* v2 step-3c prereq: optional `tessera.chunk_size=<bytes>` mount
	 * option pins the chunk granularity for every chunked write on
	 * this mount. Must be a power-of-two in [4096, 4194304]; reject
	 * the mount with EINVAL otherwise so misconfigurations are loud
	 * rather than silently auto-tiered. */
	uint32_t chunk_size_override = 0;
	{
		int cs_err;
		char *cs_str = vfs_getopts(mp->mnt_optnew, "tessera.chunk_size",
		    &cs_err);
		if (cs_err == 0 && cs_str != NULL) {
			uint64_t v = 0;
			int bad = 0;
			for (const char *p = cs_str; *p; p++) {
				if (*p < '0' || *p > '9') { bad = 1; break; }
				v = v * 10u + (uint64_t)(*p - '0');
				if (v > (uint64_t)0xffffffffu) { bad = 1; break; }
			}
			if (bad || v < 4096u || v > (4u * 1024u * 1024u) ||
			    (v & (v - 1)) != 0) {
				printf("tessera_fs: tessera.chunk_size=%s rejected "
				    "(want power-of-2 in [4096, 4194304])\n",
				    cs_str);
				return (EINVAL);
			}
			chunk_size_override = (uint32_t)v;
		}
	}

	/* Optional `tessera.quota_bytes=N` — activate a whole-FS quota with
	 * an N-byte logical limit (tessera-quotas.md). 0/absent = no quota.
	 * Reject a non-numeric value loudly. */
	uint64_t quota_bytes = 0;
	{
		int q_err;
		char *q_str = vfs_getopts(mp->mnt_optnew, "tessera.quota_bytes",
		    &q_err);
		if (q_err == 0 && q_str != NULL) {
			uint64_t v = 0;
			int bad = (*q_str == '\0');
			for (const char *p = q_str; *p; p++) {
				if (*p < '0' || *p > '9') { bad = 1; break; }
				if (v > (UINT64_MAX - 9) / 10) { bad = 1; break; }
				v = v * 10u + (uint64_t)(*p - '0');
			}
			if (bad) {
				printf("tessera_fs: tessera.quota_bytes=%s rejected "
				    "(want a non-negative integer)\n", q_str);
				return (EINVAL);
			}
			quota_bytes = v;
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

	err = tessera_mountfs(devvp, mp, requested_gen, chunk_size_override,
	    quota_bytes);
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

	if (tessera_singleton_mount == tmp_)
		tessera_singleton_mount = NULL;

	if (tmp_ != NULL) {
		/* v2-step-2a: stop accepting new flushes, drain any in-flight
		 * callout / task, then do one final synchronous flush so the
		 * SB on disk reflects every committed mutation before tear-
		 * down. After this point sb_dirty must be 0. */
		if (tmp_->flush_co_init) {
			tmp_->flush_unmounting = 1;
			callout_drain(&tmp_->flush_co);
			taskqueue_drain(taskqueue_thread, &tmp_->flush_task);
			/* v2.6 B.2: stop the journal-log callout + drain its
			 * task, then force a final synchronous drain so any
			 * unjournaled ops become durable before we close
			 * the journal. */
			if (tmp_->journal_log_co_init) {
				callout_drain(&tmp_->journal_log_co);
				taskqueue_drain(taskqueue_thread,
				    &tmp_->journal_log_task);
				(void)tessera_fs_journal_log_drain(tmp_);
				tmp_->journal_log_co_init = 0;
			}
			(void)tessera_fs_flush(tmp_);
		}
		if (tmp_->repack_task_init) {
			taskqueue_drain(taskqueue_thread, &tmp_->repack_task);
			tmp_->repack_task_init = 0;
		}
		/* Drain any leftover dirty inodes (best-effort) — the
		 * final flush above should have cleared them, but if
		 * inode_tree was NULL the writes accumulated and we now
		 * just leak them. */
		if (tmp_->dirty_init) {
			for (uint32_t _b = 0;
			    _b < TESSERA_DIRTY_INODE_BUCKETS; _b++) {
				struct tessera_dirty_inode *_e;
				while ((_e = LIST_FIRST(&tmp_->dirty_inodes[_b]))
				    != NULL) {
					LIST_REMOVE(_e, link);
					free(_e, M_TESSERA);
				}
			}
			for (uint32_t _b = 0;
			    _b < TESSERA_PENDING_MANIFEST_BUCKETS; _b++) {
				struct tessera_pending_manifest *_pm;
				while ((_pm = LIST_FIRST(
				    &tmp_->pending_manifests[_b])) != NULL) {
					LIST_REMOVE(_pm, link);
					struct tessera_pending_owner *_po;
					while ((_po = LIST_FIRST(&_pm->owners))
					    != NULL) {
						LIST_REMOVE(_po, link);
						free(_po, M_TESSERA);
					}
					free(_pm->bytes, M_TESSERA);
					free(_pm, M_TESSERA);
				}
			}
			for (uint32_t _b = 0;
			    _b < TESSERA_DIRENT_LOG_BUCKETS; _b++) {
				struct tessera_dirent_log_entry *_de;
				while ((_de = LIST_FIRST(
				    &tmp_->dirent_log[_b])) != NULL) {
					LIST_REMOVE(_de, link);
					free(_de, M_TESSERA);
				}
			}
			for (uint32_t _b = 0;
			    _b < TESSERA_DIRTY_CONTENT_BUCKETS; _b++) {
				struct tessera_dirty_content *_dc;
				while ((_dc = LIST_FIRST(
				    &tmp_->dirty_content[_b])) != NULL) {
					LIST_REMOVE(_dc, link);
					if (_dc->bytes != NULL)
						free(_dc->bytes, M_TESSERA);
					free(_dc, M_TESSERA);
				}
			}
			tmp_->dirty_content_bytes = 0;
			tessera_cas_cache_drain(&tmp_->cas_cache);
			/* Drain any still-queued journal-pending entries.
			 * The synchronous drain above should have cleared
			 * them, but flush_unmounting / closed-journal paths
			 * leave them hanging — free here. */
			{
				struct tessera_dirent_log_entry *_jp;
				while ((_jp = LIST_FIRST(
				    &tmp_->journal_pending)) != NULL) {
					LIST_REMOVE(_jp, link);
					free(_jp, M_TESSERA);
				}
				tmp_->journal_pending_count = 0;
			}
			tmp_->dirty_init = 0;
		}
		if (tmp_->flush_mtx_init) {
			mtx_destroy(&tmp_->flush_mtx);
			tmp_->flush_mtx_init = 0;
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
		if (tmp_->meta_free       != NULL) free(tmp_->meta_free, M_TESSERA);
		if (tmp_->meta_pending    != NULL) free(tmp_->meta_pending, M_TESSERA);
		if (tmp_->meta_pin_bitmap != NULL) free(tmp_->meta_pin_bitmap, M_TESSERA);
		/* Drain any still-dirty buffers on devvp BEFORE detaching
		 * the GEOM consumer. tessera_kbio_write_delayed uses
		 * bdwrite — buffers stay on devvp's bufobj.bo_dirty list
		 * until sched_sync (or our own VOP_FSYNC) writes them out.
		 * If we tear down the consumer first, sched_sync later
		 * panics in g_vfs_strategy on the dangling buffer. The
		 * panic was reproducible under stress2's heavy meta-zone
		 * mutation followed by umount; not seen in light
		 * workloads because bdwrite cadence stayed low enough
		 * that sched_sync always drained before unmount. */
		if (tmp_->devvp != NULL) {
			vn_lock(tmp_->devvp, LK_EXCLUSIVE | LK_RETRY);
			(void)VOP_FSYNC(tmp_->devvp, MNT_WAIT, curthread);
			VOP_UNLOCK(tmp_->devvp);
		}
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
	/* Live regular vnodes: kind=REGULAR, snapshot_gen=0. Match on
	 * inode_no alone (existing semantics). For magic-dir / snapshot
	 * vnodes (created via tessera_vget_synth), the cmp is done via
	 * tessera_inode_cmp_ex which checks all three fields. */
	if (tn->kind != TESSERA_NODE_REGULAR || tn->snapshot_gen != 0)
		return (1); /* never match; force fresh creation */
	return (tn->inode_no == target) ? 0 : 1;
}

/* v2 slice-3 composite-key cmp for magic-dir and snapshot vnodes.
 * vfs_hash uses a u_int hash but the cmp can dedup on whatever
 * fields we want. Using vfs_hash for synth vnodes (instead of
 * minting fresh ones every lookup) lets unmount's vflush properly
 * track + reclaim them. */
struct tessera_vget_key {
	uint64_t inode_no;
	uint64_t snapshot_gen;
	uint8_t  kind;
};
static int
tessera_inode_cmp_ex(struct vnode *vp, void *arg)
{
	struct tessera_vget_key *k = (struct tessera_vget_key *)arg;
	struct tessera_node *tn = VTOTNODE(vp);
	if (tn == NULL) return (1);
	return (tn->inode_no == k->inode_no &&
	    tn->snapshot_gen == k->snapshot_gen &&
	    tn->kind == k->kind) ? 0 : 1;
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
		if (tessera_fs_inode_get_byk(tmp_, k4, &cino)
		    == TESSERA_OK) {
			switch (cino.mode & 0170000) {
			case 0040000: vp->v_type = VDIR;  break;
			case 0100000: vp->v_type = VREG;  break;
			case 0120000: vp->v_type = VLNK;  break;
			case 0140000: vp->v_type = VSOCK; break;
			case 0010000: vp->v_type = VFIFO; break;
			case 0020000: vp->v_type = VCHR;  break;
			case 0060000: vp->v_type = VBLK;  break;
			default:      vp->v_type = VBAD;  break;
			}
		}
	}
	if (inode_no == TESSERA_INODE_ROOT_DIR) {
		vp->v_type = VDIR;
		vp->v_vflag |= VV_ROOT;
	}

	VN_LOCK_ASHARE(vp);
	/* insmntque (dtr=true) handles its own vgone+vput on failure;
	 * insmntque1 (dtr=false) needs manual cleanup by the caller, but
	 * doing it under heavy parallel load races with the EXCLUSIVE
	 * lock acquired at vn_lock above and panics vgonel with "vnode
	 * is not exclusive locked but should be" (stress2's parallel
	 * mkdir testcase). Using insmntque side-steps the manual
	 * cleanup entirely. */
	if (insmntque(vp, mp) != 0) {
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
		/* Race: another thread won the slot. vfs_hash_insert already
		 * called vgone(vp) + vput(vp) on our losing vnode (see
		 * sys/kern/vfs_hash.c — the LIST_INSERT_HEAD-then-vgone-vput
		 * branch). Calling them again here was a double-free that
		 * panicked vgonel with "vnode is not exclusive locked but
		 * should be" under stress2's parallel mkdir testcase. */
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

/*
 * v2 slice-3: vnodes for the magic-dir tree and snapshot views.
 *
 * Goes through vfs_hash like regular vnodes — this is what makes
 * unmount's vflush correctly drain them. The hash key mixes
 * inode_no with snapshot_gen + kind so two inode-2 vnodes from
 * different snapshot gens are distinct. The hash bucket may hold
 * multiple vnodes with the same inode_no (one per gen + kind);
 * tessera_inode_cmp_ex picks the right one.
 *
 * For TESSERA_NODE_REGULAR with snapshot_gen != 0, reads the
 * snapshot's inode record to set v_type correctly (VDIR / VREG /
 * VLNK).
 */
static int
tessera_vget_synth(struct mount *mp, uint64_t inode_no,
                   uint64_t parent_inode_no, uint8_t kind,
                   uint64_t snapshot_gen, struct vnode **vpp)
{
	struct vnode *vp = NULL;
	struct thread *td = curthread;
	struct tessera_vget_key key;
	int error;

	key.inode_no     = inode_no;
	key.snapshot_gen = snapshot_gen;
	key.kind         = kind;

	/* Mix snapshot_gen + kind into the bucket hash so multiple
	 * gens spread across buckets. inode_no still dominates. */
	u_int h = (u_int)inode_no
	    ^ (u_int)(snapshot_gen * 2654435761ULL)
	    ^ ((u_int)kind * 11u);

	error = vfs_hash_get(mp, h, LK_EXCLUSIVE, td, &vp,
	    tessera_inode_cmp_ex, &key);
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
	tn->kind            = kind;
	tn->snapshot_gen    = snapshot_gen;
	vp->v_data = tn;

	if (kind == TESSERA_NODE_MAGIC_TESSERA ||
	    kind == TESSERA_NODE_MAGIC_SNAPSHOTS) {
		vp->v_type = VDIR;
	} else if (snapshot_gen != 0) {
		struct tessera_mount *tmp_ = VFSTOTESSERA(mp);
		tessera_inode_record_t cino;
		if (tessera_fs_inode_get_at_gen(tmp_, (uint32_t)inode_no,
		    snapshot_gen, &cino) == TESSERA_OK) {
			switch (cino.mode & 0170000) {
			case 0040000: vp->v_type = VDIR;  break;
			case 0100000: vp->v_type = VREG;  break;
			case 0120000: vp->v_type = VLNK;  break;
			case 0140000: vp->v_type = VSOCK; break;
			case 0010000: vp->v_type = VFIFO; break;
			case 0020000: vp->v_type = VCHR;  break;
			case 0060000: vp->v_type = VBLK;  break;
			default:      vp->v_type = VBAD;  break;
			}
		} else {
			vp->v_type = VBAD;
		}
	} else {
		vp->v_type = VNON;
	}

	VN_LOCK_ASHARE(vp);
	/* insmntque (dtr=true) handles its own vgone+vput on failure;
	 * insmntque1 (dtr=false) needs manual cleanup by the caller, but
	 * doing it under heavy parallel load races with the EXCLUSIVE
	 * lock acquired at vn_lock above and panics vgonel with "vnode
	 * is not exclusive locked but should be" (stress2's parallel
	 * mkdir testcase). Using insmntque side-steps the manual
	 * cleanup entirely. */
	if (insmntque(vp, mp) != 0) {
		free(tn, M_TESSERA);
		return (EIO);
	}

	struct vnode *other = NULL;
	error = vfs_hash_insert(vp, h, LK_EXCLUSIVE, td, &other,
	    tessera_inode_cmp_ex, &key);
	if (error != 0) {
		vput(vp);
		return (error);
	}
	if (other != NULL) {
		/* Another thread won the slot. vfs_hash_insert already did
		 * vgone+vput on our losing vnode — see tessera_vget for the
		 * detailed comment. */
		*vpp = other;
		return (0);
	}

	vn_set_state(vp, VSTATE_CONSTRUCTED);
	*vpp = vp;
	return (0);
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
	/* Quota-scoped statfs (tessera-quotas.md §3.6): inside a quota'd
	 * mount, df reports the DOMAIN's logical numbers, not the physical
	 * pool. This is load-bearing for security as well as df sanity — the
	 * global physical free-space counter is a dedup existence oracle, so
	 * a jail-visible mount must never expose it. Available is clamped to
	 * the physical pool free so the limit can't promise bytes the pool
	 * can't deliver. */
	if (tmp_->quota_active) {
		uint64_t bs        = TESSERA_SECTOR_SIZE;
		uint64_t limit_blk = tmp_->quota_dom.limit_bytes / bs;
		uint64_t used_blk  = (tmp_->quota_dom.used_bytes + bs - 1) / bs;
		uint64_t avail_blk = (limit_blk > used_blk)
		    ? (limit_blk - used_blk) : 0;
		if (avail_blk > free_data)
			avail_blk = free_data;
		sbp->f_blocks = limit_blk;
		sbp->f_bfree  = avail_blk;
		sbp->f_bavail = avail_blk;
	}
	/* Inode count. v2.5 BTREE directory makes per-op O(log N), so
	 * we no longer need the previous 2K hard floor on f_ffree. But
	 * tessera's per-op fixed overhead (publish_manifest +
	 * inode_put + pack_registry pre-check) is still ~ms-class
	 * rather than the ~10 µs that ext4 / btrfs hit, so size-by-
	 * inodes tools (stress2's `size = ifree / incarnations`) still
	 * pick workloads that exceed our per-op cost × 100×outer-loop
	 * bound. Cap at 8K free for now — comfortably above any
	 * realistic single-dir population, well under the workload
	 * stress2 explodes into at unbounded f_ffree. Lift further
	 * after pack_registry hot-path caching lands. */
	{
		uint64_t used = (tmp_->sb.next_inode_no >
		    TESSERA_INODE_FIRST_USER) ?
		    (tmp_->sb.next_inode_no - TESSERA_INODE_FIRST_USER) : 0;
		uint64_t free_files = 8192;
		sbp->f_files  = used + free_files;
		sbp->f_ffree  = free_files;
	}
	return (0);
}

/* sync(2) handler. vfs_stdsync iterates dirty vnodes and calls
 * VOP_FSYNC on each, but tessera doesn't dirty vnodes through the
 * standard buffer cache (its content lives in dirty_content + the
 * pending-manifest cache, both invisible to the kernel). So we drain
 * those caches explicitly and run our normal flush, which commits
 * the SB. Without this, `sync` is silently a no-op for any tessera
 * write that's still buffered. */
static int
tessera_sync_impl(struct mount *mp, int waitfor)
{
	struct tessera_mount *tmp_ = VFSTOTESSERA(mp);
	if (tmp_ == NULL) return (0);
	(void)tessera_fs_dirty_content_drain_all(tmp_);
	(void)tessera_fs_flush(tmp_);
	(void)waitfor;  /* drain + flush are synchronous either way */
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
	.vfs_sync    = tessera_sync_impl,
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

	/* v2 slice-3: magic dirs are read-only; reject write access. */
	if (tn->kind != TESSERA_NODE_REGULAR) {
		if (ap->a_accmode & VWRITE) return (EROFS);
		return (0);
	}
	/* Snapshot vnodes are also read-only. */
	if (tn->snapshot_gen != 0) {
		if (ap->a_accmode & VWRITE) return (EROFS);
		/* Fall through to ordinary mode check using snapshot's
		 * inode record so r-x / r-- semantics are preserved. */
		uint8_t key[4];
		tessera_inode_record_t ino;
		encode_inode_key((uint32_t)tn->inode_no, key);
		if (tessera_fs_inode_get_at_gen(tmp_, (uint32_t)tn->inode_no,
		    tn->snapshot_gen, &ino) != TESSERA_OK)
			return (EIO);
		return (vaccess(vp->v_type, ino.mode & 07777, ino.uid,
		    ino.gid, ap->a_accmode, ap->a_cred));
	}

	uint8_t key[4];
	tessera_inode_record_t ino;
	encode_inode_key((uint32_t)tn->inode_no, key);
	if (tessera_fs_inode_get_byk(tmp_, key, &ino) != TESSERA_OK)
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

	/* v2 slice-3: magic-dir vnodes (`.tessera`, `.tessera/snapshots`)
	 * have no on-disk inode. Synthesize stable read-only-dir attrs. */
	if (tn->kind != TESSERA_NODE_REGULAR) {
		vap->va_type  = VDIR;
		vap->va_mode  = 0555;
		vap->va_nlink = 2;
		vap->va_uid   = 0;
		vap->va_gid   = 0;
		vap->va_size  = 0;
		vap->va_bytes = 0;
		vap->va_gen   = 1;
		vap->va_flags = 0;
		return (0);
	}

	/* Try to read the on-disk inode record. ENOENT fall-through means
	 * the volume hasn't had inode 2 populated yet (mkfs work pending
	 * for round 3c) — return synthesized empty-root attrs so the
	 * mount stays usable for diagnostic stat/df. */
	int read_real = 0;
	if (tmp_->inode_tree != NULL) {
		uint8_t key[4];
		tessera_inode_record_t ino;
		encode_inode_key((uint32_t)tn->inode_no, key);
		int rc = (tn->snapshot_gen != 0)
		    ? tessera_fs_inode_get_at_gen(tmp_, (uint32_t)tn->inode_no,
		          tn->snapshot_gen, &ino)
		    : tessera_fs_inode_get_byk(tmp_, key, &ino);
		if (rc == TESSERA_OK) {
			switch (ino.mode & 0170000) {
			case 040000:  vap->va_type = VDIR;  break;
			case 0100000: vap->va_type = VREG;  break;
			case 0120000: vap->va_type = VLNK;  break;
			case 0140000: vap->va_type = VSOCK; break;
			case 0010000: vap->va_type = VFIFO; break;
			case 0020000: vap->va_type = VCHR;  break;
			case 0060000: vap->va_type = VBLK;  break;
			default:      vap->va_type = VBAD;  break;
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

/* DIRECTORY_2L lookup: hash the name, binary-search the outer
 * manifest's bucket list for the bucket whose first_name_hash is
 * the largest value ≤ hash(name), then fetch that bucket's flat
 * DIRECTORY manifest and do a normal lookup. */
static int
tessera_fs_dir_2l_lookup(struct tessera_mount *tmp_,
                         const tessera_manifest_parser_t *outer,
                         const char *name, uint16_t nlen,
                         uint64_t *out_inode)
{
	const uint64_t name_h = tessera_dir_name_hash(name, nlen);
	const uint32_t n = tessera_manifest_parser_count(outer);
	if (n == 0) return (ENOENT);

	int lo = 0, hi = (int)n - 1, best = 0;
	while (lo <= hi) {
		int mid = lo + (hi - lo) / 2;
		tessera_dir_bucket_record_t br;
		if (tessera_manifest_dir_bucket_at(outer,
		    (uint32_t)mid, &br) != TESSERA_OK)
			return (EIO);
		if (br.first_name_hash <= name_h) {
			best = mid;
			lo = mid + 1;
		} else {
			hi = mid - 1;
		}
	}

	tessera_dir_bucket_record_t target;
	if (tessera_manifest_dir_bucket_at(outer, (uint32_t)best, &target)
	    != TESSERA_OK)
		return (EIO);

	uint8_t *bbuf = NULL;
	uint32_t blen = 0;
	if (tessera_fs_fetch_blob(tmp_, target.bucket_manifest_hash,
	    &bbuf, &blen) != 0) return (EIO);
	if (blen < 32) { free(bbuf, M_TESSERA); return (EIO); }
	int rc = tessera_dir_lookup_name(bbuf + 32, blen - 32,
	    name, nlen, out_inode);
	free(bbuf, M_TESSERA);
	return (rc);
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

	/* v2 slice-3: magic dir hierarchy at /.tessera/snapshots/<gen>/.
	 * Three layers of synthesized vnodes; the third (snapshot root)
	 * descends into REGULAR vnodes tagged with snapshot_gen so reads
	 * use the historical inode_tree.
	 *
	 * JAIL ISOLATION: hide the entire magic-dir tree from jailed
	 * callers. Without this, any jail with `/` access could
	 * `cd /.tessera/snapshots/<N>/` and read the WHOLE volume's
	 * historical state — including subtrees belonging to other
	 * jails or the host. Gating at the `.tessera` entry suffices:
	 * the descent paths (`snapshots`, `<gen>`) require the parent
	 * vnode (NODE_MAGIC_TESSERA) which can only be obtained via
	 * this lookup. */
	if (dn->kind == TESSERA_NODE_REGULAR && dn->snapshot_gen == 0 &&
	    dn->inode_no == TESSERA_INODE_ROOT_DIR &&
	    cnp->cn_namelen == 8 &&
	    memcmp(cnp->cn_nameptr, ".tessera", 8) == 0) {
		if (cnp->cn_cred != NULL && jailed(cnp->cn_cred))
			return (ENOENT);
		struct vnode *mvp;
		int e = tessera_vget_synth(dvp->v_mount,
		    TESSERA_MAGIC_INO_TESSERA,
		    dn->inode_no, TESSERA_NODE_MAGIC_TESSERA, 0, &mvp);
		if (e != 0) return (e);
		*vpp = mvp;
		return (0);
	}
	if (dn->kind == TESSERA_NODE_MAGIC_TESSERA &&
	    cnp->cn_namelen == 9 &&
	    memcmp(cnp->cn_nameptr, "snapshots", 9) == 0) {
		struct vnode *mvp;
		int e = tessera_vget_synth(dvp->v_mount,
		    TESSERA_MAGIC_INO_SNAPSHOTS,
		    TESSERA_MAGIC_INO_TESSERA,
		    TESSERA_NODE_MAGIC_SNAPSHOTS, 0, &mvp);
		if (e != 0) return (e);
		*vpp = mvp;
		return (0);
	}
	if (dn->kind == TESSERA_NODE_MAGIC_SNAPSHOTS) {
		/* Parse name as decimal gen; look up in snapshots_tree. */
		uint64_t g = 0;
		if (cnp->cn_namelen == 0) return (ENOENT);
		for (int i = 0; i < cnp->cn_namelen; i++) {
			char c = cnp->cn_nameptr[i];
			if (c < '0' || c > '9') return (ENOENT);
			g = g * 10ull + (uint64_t)(c - '0');
			if (g > 0xffffffffffffull) return (ENOENT);
		}
		if (tmp_->snapshots_tree == NULL) return (ENOENT);
		uint8_t skey[8];
		for (int i = 0; i < 8; i++)
			skey[i] = (uint8_t)(g >> ((7 - i) * 8));
		tessera_snapshot_record_t srec;
		if (tessera_btree_get(tmp_->snapshots_tree, skey, &srec)
		    != TESSERA_OK)
			return (ENOENT);
		struct vnode *mvp;
		int e = tessera_vget_synth(dvp->v_mount,
		    TESSERA_INODE_ROOT_DIR, /*parent*/ 0,
		    TESSERA_NODE_REGULAR, g, &mvp);
		if (e != 0) return (e);
		*vpp = mvp;
		return (0);
	}
	if (cnp->cn_flags & ISDOTDOT) {
		/* (already handled above for normal vnodes; falling through
		 * here only for magic vnodes with no real parent. Return
		 * self.) */
		if (dn->kind != TESSERA_NODE_REGULAR) {
			vref(dvp);
			*vpp = dvp;
			return (0);
		}
	}

	/* Magic dirs above this point have no further children. */
	if (dn->kind != TESSERA_NODE_REGULAR) return (ENOENT);

	/* Path-component descent into a non-directory must return ENOTDIR
	 * (POSIX). Has to come BEFORE the VOP_ACCESS(VEXEC) check below —
	 * otherwise a regular file with mode 0644 surfaces as EACCES when
	 * the lookup tries to enforce search permission. UFS handles this
	 * implicitly via dp->v_type guards in ufs_lookup; we do it
	 * explicitly. */
	if (dvp->v_type != VDIR) return (ENOTDIR);

	/* POSIX: search permission on the parent directory required.
	 * Some FreeBSD lookup paths reach VOP_LOOKUP without first
	 * calling VOP_ACCESS(VEXEC) on the parent (vfs name cache,
	 * shared-lookup paths). UFS adds the check explicitly in
	 * ufs_lookup; tessera does the same. */
	{
		int aerr = VOP_ACCESS(dvp, VEXEC, cnp->cn_cred, curthread);
		if (aerr != 0) return (aerr);
	}

	/* Real on-disk lookup: read the directory inode, fetch its
	 * DIRECTORY manifest blob, walk it for `cnp`. Snapshot-tagged
	 * vnodes (snapshot_gen != 0) read the inode + manifest from
	 * the historical inode_tree. */
	uint8_t key[4];
	tessera_inode_record_t dino;
	encode_inode_key((uint32_t)dn->inode_no, key);
	int igrc = (dn->snapshot_gen != 0)
	    ? tessera_fs_inode_get_at_gen(tmp_, (uint32_t)dn->inode_no,
	          dn->snapshot_gen, &dino)
	    : tessera_fs_inode_get_byk(tmp_, key, &dino);
	if (igrc != TESSERA_OK)
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
	const tessera_manifest_kind_t dkind = tessera_manifest_parser_kind(p);
	if (dkind != TESSERA_MFT_DIRECTORY &&
	    dkind != TESSERA_MFT_DIRECTORY_2L &&
	    dkind != TESSERA_MFT_DIRECTORY_BTREE) {
		tessera_manifest_parser_free(p);
		free(blob, M_TESSERA);
		return (ENOTDIR);
	}

	uint64_t child_no = 0;
	int rc;
	/* v2.6 log read merge: consult the dirent log before any BTREE
	 * descent. The log holds the most-recent ops not yet
	 * checkpointed; ADD wins over BTREE state, REMOVE shadows the
	 * BTREE entry. Returns TESSERA_DIRENT_LOG_MISS if no entry —
	 * fall through to the on-disk lookup. */
	{
		int log_op = 0;
		uint64_t log_ino = 0;
		int lrc = tessera_fs_dirent_log_lookup(tmp_,
		    (uint32_t)dn->inode_no, cnp->cn_nameptr,
		    (uint16_t)cnp->cn_namelen, &log_op, &log_ino);
		if (lrc == 0) {
			tessera_manifest_parser_free(p);
			free(blob, M_TESSERA);
			if (log_op == 1) {
				/* REMOVE — entry logically gone. */
				if ((cnp->cn_flags & ISLASTCN) &&
				    (cnp->cn_nameiop == CREATE ||
				     cnp->cn_nameiop == RENAME))
					return (EJUSTRETURN);
				return (ENOENT);
			}
			child_no = log_ino;
			goto have_child_no;
		}
	}
	if (dkind == TESSERA_MFT_DIRECTORY) {
		/* Flat dir: walk body past the 32-byte manifest header. */
		const uint8_t *body = blob + 32;
		const size_t   blen = blob_len - 32;
		rc = tessera_dir_lookup_name(body, blen,
		    cnp->cn_nameptr, (uint16_t)cnp->cn_namelen,
		    &child_no);
	} else if (dkind == TESSERA_MFT_DIRECTORY_2L) {
		/* Two-level dir: hash → bucket → fetch bucket → flat lookup. */
		rc = tessera_fs_dir_2l_lookup(tmp_, p,
		    cnp->cn_nameptr, (uint16_t)cnp->cn_namelen,
		    &child_no);
	} else {
		/* B-tree dir: O(log N) descent. */
		rc = tessera_fs_dir_btree_lookup(tmp_, dino.manifest_hash,
		    cnp->cn_nameptr, (uint16_t)cnp->cn_namelen, &child_no);
	}
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

have_child_no:
	/* Found — return a vnode. Live mounts use the deduping
	 * tessera_vget (vfs_hash keyed on inode_no). Snapshot vnodes
	 * skip the hash because the same inode_no maps to different
	 * content across gens — vget_synth mints a fresh vnode and
	 * propagates snapshot_gen. */
	if (dn->snapshot_gen != 0) {
		return (tessera_vget_synth(dvp->v_mount, child_no,
		    dn->inode_no, TESSERA_NODE_REGULAR,
		    dn->snapshot_gen, vpp));
	}
	return (tessera_vget(dvp->v_mount, child_no, dn->inode_no, vpp));
}

#define DIRENT_HDR  offsetof(struct dirent, d_name)

static size_t
tessera_dirent_reclen(uint16_t namlen)
{
	size_t need = DIRENT_HDR + namlen + 1;
	return ((need + 7) & ~7);
}

/* Emit a dirent into uio. Returns:
 *   0      success.
 *   EAGAIN remaining uio->uio_resid is too small for this entry; the
 *          caller should stop the readdir walk WITHOUT advancing past
 *          this dirent. uio is left unchanged so the next readdir(2)
 *          call resumes here. (This is the v1-of-multi-buffer-readdir
 *          fix that big-directory workloads need.)
 *   ENAMETOOLONG the name itself is bigger than our static buffer.
 *   any  uiomove error (rare).
 */
static int
tessera_emit_dirent(struct uio *uio, ino_t fileno, uint8_t type,
                    const char *name, uint16_t namlen)
{
	/* 280 covers any TESSERA_PATH_NAME_MAX (255) plus header + NUL +
	 * 8-byte alignment slack. */
	uint8_t buf[288];
	size_t reclen = tessera_dirent_reclen(namlen);
	if (reclen > sizeof(buf)) return (ENAMETOOLONG);

	if ((size_t)uio->uio_resid < reclen)
		return (EAGAIN);

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

	/* Multi-call readdir support. uio_offset is treated as a logical
	 * directory cookie: total bytes returned by all prior readdir
	 * calls on this stream. We re-walk every call, skipping dirents
	 * whose end-position falls at or below uio_offset, and emit the
	 * rest until the user buffer is full (tessera_emit_dirent returns
	 * EAGAIN). This is enough for ls(1) / find(1) / fts(3) with
	 * arbitrary directory size; sequential walks are O(N) per call
	 * but acceptable for typical dir sizes. */
	const off_t resume_at = uio->uio_offset;
	off_t walked = 0;
	int   stopped = 0;

	/* Skip-or-emit one (fileno, dt, name, nlen) entry. EAGAIN from
	 * the emitter signals end-of-buffer; we stop the walk WITHOUT
	 * setting eofflag so the next readdir(2) call resumes here. */
#define _EMIT(_fno, _dt, _name, _nlen) do {                               \
	size_t _rl = tessera_dirent_reclen(_nlen);                        \
	if (walked + (off_t)_rl <= resume_at) {                           \
		walked += (off_t)_rl;                                     \
		break;                                                    \
	}                                                                 \
	err = tessera_emit_dirent(uio, _fno, _dt, _name, _nlen);          \
	if (err == EAGAIN) { err = 0; stopped = 1; goto stop_walk; }      \
	if (err != 0) goto stop_walk;                                     \
	walked += (off_t)_rl;                                             \
} while (0)

	_EMIT(tn->inode_no, DT_DIR, ".", 1);
	_EMIT(tn->inode_no, DT_DIR, "..", 2);

	/* v2 slice-3: synthesized magic-dir contents. d_fileno must be
	 * non-zero (zero is a "deleted entry" tombstone for ls). */
	if (tn->kind == TESSERA_NODE_MAGIC_TESSERA) {
		_EMIT(TESSERA_MAGIC_INO_SNAPSHOTS, DT_DIR, "snapshots", 9);
		goto stop_walk;
	}
	if (tn->kind == TESSERA_NODE_MAGIC_SNAPSHOTS) {
		/* Iterate snapshots_tree, emit one entry per retained gen.
		 *
		 * IMPORTANT: _EMIT's EAGAIN path does `goto stop_walk` —
		 * which would jump OUT of this block while the btree cursor
		 * is still allocated. Leaked cursor → leaked btree handle →
		 * leaked mount reference → unmount hangs.
		 *
		 * Defence: snapshot the gens to a local array first (cursor
		 * scope strictly bounded), then loop emit. snapshots are
		 * naturally bounded by retention, so the array is small. */
		if (tmp_->snapshots_tree == NULL) goto stop_walk;
		uint64_t gens[64];
		uint32_t ngens = 0;
		tessera_btree_cursor_t *sc =
		    tessera_btree_seek_first(tmp_->snapshots_tree);
		while (sc != NULL && ngens < 64) {
			uint8_t skey[8];
			tessera_snapshot_record_t srec;
			if (tessera_btree_cursor_get(sc, skey, &srec)
			    != TESSERA_OK)
				break;
			gens[ngens++] = srec.generation;
			if (tessera_btree_cursor_next(sc) != TESSERA_OK) break;
		}
		if (sc != NULL) tessera_btree_cursor_free(sc);
		for (uint32_t gi = 0; gi < ngens && !stopped; gi++) {
			char buf[24];
			int n = snprintf(buf, sizeof buf, "%lu",
			    (unsigned long)gens[gi]);
			if (n > 0 && n < (int)sizeof buf) {
				_EMIT((ino_t)gens[gi], DT_DIR, buf,
				    (uint16_t)n);
			}
		}
		goto stop_walk;
	}

	/* Walk the directory's manifest, if any. */
	if (tmp_->inode_tree == NULL) goto stop_walk;

	/* v2.6 dirent log: force a checkpoint of THIS parent before
	 * walking the BTREE. The log holds in-flight dirent ops that
	 * aren't reflected in the BTREE yet; the cleanest way to feed
	 * readdir a coherent view is to flush the log into the BTREE
	 * first. After this, the BTREE is the authoritative state and
	 * the log is empty for this parent. Subsequent readdir
	 * iterations (multi-call cookie protocol) re-checkpoint
	 * cheaply (no log entries to apply). */
	if (tn->snapshot_gen == 0)
		(void)tessera_fs_dirent_log_checkpoint_parent(tmp_,
		    (uint32_t)tn->inode_no);

	uint8_t key[4];
	tessera_inode_record_t dino;
	encode_inode_key((uint32_t)tn->inode_no, key);
	int igrc2 = (tn->snapshot_gen != 0)
	    ? tessera_fs_inode_get_at_gen(tmp_, (uint32_t)tn->inode_no,
	          tn->snapshot_gen, &dino)
	    : tessera_fs_inode_get_byk(tmp_, key, &dino);
	if (igrc2 != TESSERA_OK) goto stop_walk;
	if (tessera_hash_is_null(dino.manifest_hash))
		goto stop_walk;

	/* For DT_UNKNOWN inode lookups inside the body walker, use the
	 * same gen-aware path so child d_type is correct. We override
	 * the macro's inner inode_get to consult snapshot_gen too —
	 * achieved by passing tn->snapshot_gen into the body walker
	 * via a local. */
	const uint64_t _rd_gen = tn->snapshot_gen;
	(void)_rd_gen;  /* used inside _RD_EMIT_BODY below if non-zero */

	uint8_t *blob = NULL;
	uint32_t blob_len = 0;
	if (tessera_fs_fetch_blob(tmp_, dino.manifest_hash, &blob, &blob_len)
	    != 0)
		goto stop_walk;

	tessera_manifest_parser_t *p = tessera_manifest_parse(blob, blob_len);
	if (p == NULL) {
		free(blob, M_TESSERA);
		goto stop_walk;
	}
	const tessera_manifest_kind_t rkind = tessera_manifest_parser_kind(p);
	if (rkind != TESSERA_MFT_DIRECTORY &&
	    rkind != TESSERA_MFT_DIRECTORY_2L &&
	    rkind != TESSERA_MFT_DIRECTORY_BTREE) {
		tessera_manifest_parser_free(p);
		free(blob, M_TESSERA);
		goto stop_walk;
	}

	/* Inline body walker: skip-or-emit each (child, name, kind). */
#define _RD_EMIT_BODY(_body, _blen) do {                                  \
	for (size_t _off = 0; _off + 10 <= (_blen); ) {                   \
		uint64_t _ch;                                             \
		uint16_t _nl;                                             \
		memcpy(&_ch, (_body) + _off,     8);                      \
		memcpy(&_nl, (_body) + _off + 8, 2);                      \
		if (_off + 10 + _nl > (_blen)) break;                     \
		const char *_nm = (const char *)((_body) + _off + 10);    \
		uint8_t _dt = DT_UNKNOWN;                                 \
		uint8_t _k2[4];                                           \
		tessera_inode_record_t _cino;                             \
		encode_inode_key((uint32_t)_ch, _k2);                     \
		int _crc = (_rd_gen != 0)                                 \
		    ? tessera_fs_inode_get_at_gen(tmp_, (uint32_t)_ch,    \
		          _rd_gen, &_cino)                                \
		    : tessera_fs_inode_get_byk(tmp_, _k2, &_cino);        \
		if (_crc == TESSERA_OK)                                   \
			_dt = tessera_dt_from_mode(_cino.mode);           \
		_EMIT(_ch, _dt, _nm, _nl);                                \
		_off += 10 + _nl;                                         \
	}                                                                 \
} while (0)

#define _RD_EMIT_BTREE_LEAF(_body, _blen) do {                            \
	for (size_t _off = 0; _off + 18 <= (_blen); ) {                   \
		uint64_t _ch;                                             \
		uint16_t _nl;                                             \
		memcpy(&_ch, (_body) + _off + 8, 8);                      \
		memcpy(&_nl, (_body) + _off + 16, 2);                     \
		if (_off + 18 + _nl > (_blen)) break;                     \
		const char *_nm = (const char *)((_body) + _off + 18);    \
		uint8_t _dt = DT_UNKNOWN;                                 \
		uint8_t _k2[4];                                           \
		tessera_inode_record_t _cino;                             \
		encode_inode_key((uint32_t)_ch, _k2);                     \
		int _crc = (_rd_gen != 0)                                 \
		    ? tessera_fs_inode_get_at_gen(tmp_, (uint32_t)_ch,    \
		          _rd_gen, &_cino)                                \
		    : tessera_fs_inode_get_byk(tmp_, _k2, &_cino);        \
		if (_crc == TESSERA_OK)                                   \
			_dt = tessera_dt_from_mode(_cino.mode);           \
		_EMIT(_ch, _dt, _nm, _nl);                                \
		_off += 18 + _nl;                                         \
	}                                                                 \
} while (0)

	if (rkind == TESSERA_MFT_DIRECTORY) {
		const uint8_t *body = blob + 32;
		const size_t   blen = blob_len - 32;
		_RD_EMIT_BODY(body, blen);
	} else if (rkind == TESSERA_MFT_DIRECTORY_BTREE) {
		/* DFS the b-tree. Bounded depth (log_F N), iterate leaves
		 * left-to-right via an explicit stack of node hashes +
		 * indices so we don't blow recursion. With FANOUT_INNER=64
		 * even an N=4M dir is depth ~4. */
		struct {
			tessera_hash_t hash;
			uint8_t *bbuf;
			uint32_t blen2;
			int      leaf;
			const uint8_t *body;
			size_t   body_len;
			uint32_t count;
			uint32_t idx;
		} stack[16];
		int sp = 0;
		memcpy(stack[0].hash, dino.manifest_hash, TESSERA_HASH_SIZE);
		stack[0].bbuf = blob;          /* root: reuse already-fetched */
		stack[0].blen2 = blob_len;
		int decoded = tessera_fs_dir_btree_decode(blob, blob_len,
		    &stack[0].leaf, &stack[0].body, &stack[0].body_len,
		    &stack[0].count);
		stack[0].idx = 0;
		if (decoded != 0) { err = EIO; goto out_free; }
		blob = NULL;  /* ownership moved into stack[0].bbuf */
		while (sp >= 0 && !stopped) {
			if (stack[sp].leaf) {
				_RD_EMIT_BTREE_LEAF(stack[sp].body,
				    stack[sp].body_len);
				if (sp == 0) free(stack[sp].bbuf, M_TESSERA);
				else        free(stack[sp].bbuf, M_TESSERA);
				stack[sp].bbuf = NULL;
				sp--;
				continue;
			}
			if (stack[sp].idx >= stack[sp].count) {
				if (sp == 0) free(stack[sp].bbuf, M_TESSERA);
				else        free(stack[sp].bbuf, M_TESSERA);
				stack[sp].bbuf = NULL;
				sp--;
				continue;
			}
			size_t coff = (size_t)stack[sp].idx *
			    (8 + TESSERA_HASH_SIZE);
			tessera_hash_t child;
			memcpy(child, stack[sp].body + coff + 8,
			    TESSERA_HASH_SIZE);
			stack[sp].idx++;
			if (sp + 1 >= 16) { err = EIO; goto out_free; }
			sp++;
			memcpy(stack[sp].hash, child, TESSERA_HASH_SIZE);
			stack[sp].bbuf = NULL;
			stack[sp].blen2 = 0;
			stack[sp].idx = 0;
			if (tessera_fs_fetch_blob(tmp_, child,
			    &stack[sp].bbuf, &stack[sp].blen2) != 0) {
				err = EIO;
				while (sp >= 0) {
					if (stack[sp].bbuf)
						free(stack[sp].bbuf, M_TESSERA);
					sp--;
				}
				goto out_free_btree;
			}
			if (tessera_fs_dir_btree_decode(stack[sp].bbuf,
			    stack[sp].blen2, &stack[sp].leaf,
			    &stack[sp].body, &stack[sp].body_len,
			    &stack[sp].count) != 0) {
				err = EIO;
				while (sp >= 0) {
					if (stack[sp].bbuf)
						free(stack[sp].bbuf, M_TESSERA);
					sp--;
				}
				goto out_free_btree;
			}
		}
		/* Drain any still-allocated frames on early-stop. */
		while (sp >= 0) {
			if (stack[sp].bbuf) free(stack[sp].bbuf, M_TESSERA);
			sp--;
		}
out_free_btree:
		tessera_manifest_parser_free(p);
		goto stop_walk;
	} else {
		/* DIRECTORY_2L: iterate buckets, fetch each, walk it. */
		const uint32_t nbk = tessera_manifest_parser_count(p);
		for (uint32_t bi = 0; bi < nbk && !stopped; bi++) {
			tessera_dir_bucket_record_t br;
			if (tessera_manifest_dir_bucket_at(p, bi, &br)
			    != TESSERA_OK) { err = EIO; goto out_free; }
			uint8_t *bbuf = NULL;
			uint32_t blen2 = 0;
			if (tessera_fs_fetch_blob(tmp_,
			    br.bucket_manifest_hash, &bbuf, &blen2) != 0) {
				err = EIO; goto out_free;
			}
			if (blen2 < 32) {
				free(bbuf, M_TESSERA);
				err = EIO; goto out_free;
			}
			const uint8_t *body = bbuf + 32;
			const size_t   blen = blen2 - 32;
			_RD_EMIT_BODY(body, blen);
			free(bbuf, M_TESSERA);
		}
	}
#undef _RD_EMIT_BODY
#undef _RD_EMIT_BTREE_LEAF
#undef _EMIT

out_free:
	tessera_manifest_parser_free(p);
	free(blob, M_TESSERA);

stop_walk:
	if (err != 0) return (err);
	if (!stopped && ap->a_eofflag != NULL) *ap->a_eofflag = 1;
	return (0);
}

/* v2 step-3c: read recursively into uio for CHUNK_LIST and CHUNK_TREE
 * manifests. CHUNK_LIST does the actual chunk-by-chunk uiomove (the
 * logic that used to live inline in vop_read). CHUNK_TREE walks
 * tree_records and recurses into each child; sub-children may
 * themselves be CHUNK_LIST or another CHUNK_TREE level. */
static int
tessera_fs_read_into_uio(struct tessera_mount *tmp_,
                        tessera_manifest_parser_t *p,
                        struct uio *uio)
{
	const tessera_manifest_kind_t k = tessera_manifest_parser_kind(p);

	if (k == TESSERA_MFT_CHUNK_LIST) {
		const uint32_t n = tessera_manifest_parser_count(p);
		for (uint32_t i = 0; i < n && uio->uio_resid > 0; i++) {
			tessera_chunk_record_t cr;
			if (tessera_manifest_chunk_at(p, i, &cr)
			    != TESSERA_OK)
				return (EIO);
			const uint64_t cstart = cr.logical_offset;
			const uint64_t cend   = cstart + cr.uncompressed_size;

			if (cend <= (uint64_t)uio->uio_offset) continue;
			if (cstart >= (uint64_t)uio->uio_offset
			    + (uint64_t)uio->uio_resid) break;

			const uint64_t lo = ((uint64_t)uio->uio_offset > cstart)
			    ? (uint64_t)uio->uio_offset - cstart : 0;
			const uint64_t hi_off =
			    (uint64_t)uio->uio_offset + (uint64_t)uio->uio_resid;
			const uint64_t hi = (hi_off < cend
			    ? hi_off - cstart : cr.uncompressed_size);
			const size_t   n_copy = (size_t)(hi - lo);

			if (cr.flags & TESSERA_CHUNK_FLAG_ZERO_HOLE) {
				uint8_t *zb = malloc(n_copy, M_TESSERA,
				    M_WAITOK | M_ZERO);
				int err = uiomove(zb, n_copy, uio);
				free(zb, M_TESSERA);
				if (err != 0) return (err);
				continue;
			}

			uint8_t *cb = NULL;
			uint32_t cb_len = 0;
			if (tessera_fs_fetch_blob(tmp_, cr.chunk_hash,
			    &cb, &cb_len) != 0) return (EIO);
			if (cb_len < cr.uncompressed_size) {
				free(cb, M_TESSERA);
				return (EIO);
			}
			int err = uiomove(cb + lo, n_copy, uio);
			free(cb, M_TESSERA);
			if (err != 0) return (err);
		}
		return (0);
	}

	if (k == TESSERA_MFT_CHUNK_TREE) {
		const uint32_t n = tessera_manifest_parser_count(p);
		const uint64_t total = tessera_manifest_parser_size(p);
		for (uint32_t i = 0; i < n && uio->uio_resid > 0; i++) {
			tessera_tree_record_t tr;
			if (tessera_manifest_tree_at(p, i, &tr) != TESSERA_OK)
				return (EIO);
			const uint64_t cstart = tr.logical_offset;
			/* Use the next sibling's offset (or the parent's
			 * total logical_size for the last child) as this
			 * subtree's exclusive upper bound. Sub-manifests
			 * whose own headers state a smaller logical_size
			 * reported by themselves are still bounded; this
			 * is just for early-out pruning of the read window. */
			uint64_t cend;
			if (i + 1 < n) {
				tessera_tree_record_t next;
				if (tessera_manifest_tree_at(p, i + 1, &next)
				    != TESSERA_OK) return (EIO);
				cend = next.logical_offset;
			} else {
				cend = (total > cstart) ? total : cstart;
			}

			if (cend <= (uint64_t)uio->uio_offset) continue;
			if (cstart >= (uint64_t)uio->uio_offset
			    + (uint64_t)uio->uio_resid) break;

			uint8_t *cblob = NULL;
			uint32_t cblob_len = 0;
			if (tessera_fs_fetch_blob(tmp_, tr.child_manifest_hash,
			    &cblob, &cblob_len) != 0) return (EIO);
			tessera_manifest_parser_t *cp =
			    tessera_manifest_parse(cblob, cblob_len);
			if (cp == NULL) {
				free(cblob, M_TESSERA);
				return (EIO);
			}
			int err = tessera_fs_read_into_uio(tmp_, cp, uio);
			tessera_manifest_parser_free(cp);
			free(cblob, M_TESSERA);
			if (err != 0) return (err);
		}
		return (0);
	}

	return (EIO);
}

/*
 * Read content from an inode into a uio. Same body that backs
 * tessera_vop_read; also used by tessera_vop_getpages to fill VM pages
 * directly without going through the buffer-cache strategy path.
 */
static int
tessera_fs_read_inode_uio(struct tessera_mount *tmp_,
                          const tessera_inode_record_t *ino,
                          struct uio *uio)
{
	int err = 0;

	if ((uint64_t)uio->uio_offset >= ino->size) return (0);
	if (tessera_hash_is_null(ino->manifest_hash)) return (0);

	uint8_t *blob = NULL;
	uint32_t blob_len = 0;
	if (tessera_fs_fetch_blob(tmp_, ino->manifest_hash,
	    &blob, &blob_len) != 0)
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
		if (data_len > ino->size) data_len = (size_t)ino->size;
		if ((uint64_t)uio->uio_offset >= data_len) goto out;

		size_t remaining = data_len - (size_t)uio->uio_offset;
		size_t n = (uio->uio_resid < (ssize_t)remaining)
		    ? (size_t)uio->uio_resid : remaining;
		err = uiomove(__DECONST(void *, data + uio->uio_offset),
		    n, uio);
	} else if (kind == TESSERA_MFT_CHUNK_LIST ||
	           kind == TESSERA_MFT_CHUNK_TREE) {
		err = tessera_fs_read_into_uio(tmp_, p, uio);
	} else {
		err = EIO;
	}
out:
	tessera_manifest_parser_free(p);
	free(blob, M_TESSERA);
	return (err);
}

static int
tessera_vop_read(struct vop_read_args *ap)
{
	struct vnode *vp = ap->a_vp;
	struct uio   *uio = ap->a_uio;
	struct tessera_node *tn = VTOTNODE(vp);
	struct tessera_mount *tmp_ = VFSTOTESSERA(vp->v_mount);

	if (vp->v_type == VDIR) return (EISDIR);
	if (vp->v_type != VREG && vp->v_type != VNON) return (EINVAL);
	if (uio->uio_offset < 0) return (EINVAL);
	if (uio->uio_resid == 0) return (0);
	if (tmp_->inode_tree == NULL) return (EIO);

	uint8_t key[4];
	tessera_inode_record_t ino;
	encode_inode_key((uint32_t)tn->inode_no, key);
	int igrc3 = (tn->snapshot_gen != 0)
	    ? tessera_fs_inode_get_at_gen(tmp_, (uint32_t)tn->inode_no,
	          tn->snapshot_gen, &ino)
	    : tessera_fs_inode_get_byk(tmp_, key, &ino);
	if (igrc3 != TESSERA_OK)
		return (EIO);

	/* Live mounts may have a coalesced write buffer for this inode
	 * that hasn't been published yet — serve from there if present.
	 * Snapshot reads (gen != 0) bypass the buffer (it always
	 * reflects the live frontier). */
	if (tn->snapshot_gen == 0) {
		int br = tessera_fs_dirty_content_read(tmp_,
		    (uint32_t)tn->inode_no, uio);
		if (br > 0) return (0);
		if (br < 0) return (-br);
	}

	return (tessera_fs_read_inode_uio(tmp_, &ino, uio));
}

/* ── vop_setattr (utimes / chmod / chown / chflags) ─────────── */

/*
 * vop_setattr — utimes / chmod / chown / truncate.
 *
 * (`chflags` is not yet wired — no consumer exercises it; the field
 * isn't represented in `tessera_inode_record_t` and would need a
 * format-version bump.)
 *
 * Path: tessera_fs_inode_get (live record) → optionally rebuild
 * content via replace_content* if va_size changed → patch
 * atime/mtime/ctime/mode/uid/gid → tessera_fs_inode_put (COW through
 * meta_bio; v2 step-2b stages into dirty_inodes, drained on flush) →
 * mark_dirty (deferred SB commit). Any change persists across
 * umount/remount via the normal commit_sb path.
 *
 * Truncate to a smaller size: read-truncate-republish (via
 * replace_content / replace_content_chunked depending on file size).
 * Truncate to a larger size: zero-pad in RAM and republish. Both
 * use the same chunked-vs-INLINE dispatch logic as vop_write, so
 * truncating a 2 GiB file to 1 GiB at cs=4 KiB does the right
 * thing (CHUNK_TREE re-emit, group-level dedup at the pack registry).
 *
 * Historical note: the "deadlock during touch" investigated through
 * round 6c-redux was a kernel stack overflow in btree_put (4 KiB
 * stack arrays per recursion level vs FreeBSD aarch64's 16 KiB
 * kstack). Fix: btree.c put-path now heap-allocates its node buffers.
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

	tessera_inode_record_t ino;
	if (tessera_fs_inode_get(tmp_, (uint32_t)tn->inode_no, &ino)
	    != TESSERA_OK)
		return (EIO);

	/* POSIX permission gate — match UFS's behaviour. The kernel calls
	 * VOP_ACCESS(VWRITE) before VOP_SETATTR for the truncate path, so
	 * we don't need an explicit check there. chmod/chown have their
	 * own ownership rules: */
	struct ucred *cred = ap->a_cred;

	if (seen_uid || seen_gid) {
		uid_t new_uid = seen_uid ? vap->va_uid : ino.uid;
		gid_t new_gid = seen_gid ? vap->va_gid : ino.gid;
		/* Non-owner OR changing uid OR changing-gid-not-in-cred-groups
		 * requires PRIV_VFS_CHOWN. Mirror ufs_chown(). */
		if (cred->cr_uid != ino.uid || new_uid != ino.uid ||
		    (new_gid != ino.gid && !groupmember(new_gid, cred))) {
			int err = priv_check_cred(cred, PRIV_VFS_CHOWN);
			if (err != 0) return (err);
		}
	}
	if (seen_mode) {
		/* Non-owner needs PRIV_VFS_ADMIN to chmod. */
		if (cred->cr_uid != ino.uid) {
			int err = priv_check_cred(cred, PRIV_VFS_ADMIN);
			if (err != 0) return (err);
		}
		/* Non-root user can't set setgid on a group they aren't in
		 * (POSIX). Setuid/setgid on symlinks is allowed by FreeBSD
		 * UFS (and required by some pjdfstest cases) — we don't
		 * have FIFO/BLK/CHR types to refuse, so no EFTYPE check. */
		if (cred->cr_uid != 0 && (vap->va_mode & 02000) &&
		    !groupmember(ino.gid, cred))
			return (EPERM);
		/* Non-root user can't set the sticky bit (S_ISVTX) on a
		 * non-directory (POSIX, FreeBSD UFS — pjdfstest expects
		 * EFTYPE). */
		if (cred->cr_uid != 0 && (vap->va_mode & 01000) &&
		    vp->v_type != VDIR)
			return (EFTYPE);
	}

	/* Truncate / extend (handles `>` shell redirection's pre-write
	 * VOP_SETATTR(va_size = 0)). The new content is built in RAM by
	 * reading the existing one (capped at min(old_size, new_size))
	 * and zero-padding any extension. v1 keeps everything in RAM and
	 * always republishes as INLINE — fine for small files, will be
	 * replaced with chunked writes once vop_write goes chunked. */
	int did_resize = 0;
	if (seen_size) {
		uint64_t new_size = (uint64_t)vap->va_size;
		/* Reject impossibly-large truncates before they reach the
		 * malloc — vop_write has the same cap. Without this, a
		 * truncate(file, 1<<48) sleeps forever in M_WAITOK. */
		if (new_size > (uint64_t)(64u * 1024u * 1024u))
			return (EFBIG);
		if (new_size != ino.size) {
			/* Drain any coalesced writes so the on-disk content
			 * we're about to read reflects the latest writes. */
			(void)tessera_fs_dirty_content_drain_one(tmp_,
			    (uint32_t)tn->inode_no);
			/* Re-fetch ino post-drain (size may have grown). */
			if (tessera_fs_inode_get(tmp_,
			    (uint32_t)tn->inode_no, &ino) != TESSERA_OK)
				return (EIO);
			if (new_size == ino.size) {
				did_resize = 1;
				goto post_resize;
			}
			/* Quota: truncate-up reserves the growth (EDQUOT if it
			 * would exceed the limit); truncate-down releases the
			 * freed bytes (tessera-quotas.md §5.2-5.3). */
			if (tmp_->quota_active) {
				if (new_size > ino.size) {
					if (tessera_quota_reserve(&tmp_->quota_dom,
					    new_size - ino.size) != TESSERA_OK)
						return (EDQUOT);
				} else {
					tessera_quota_release(&tmp_->quota_dom,
					    ino.size - new_size);
				}
			}
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
			/* replace_content updated the inode record; re-fetch
			 * the live copy so atime/mtime updates below stick. */
			if (tessera_fs_inode_get(tmp_, (uint32_t)tn->inode_no,
			    &ino) != TESSERA_OK) return (EIO);
			did_resize = 1;
		}
	}
post_resize:

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
	/* POSIX: chown by a non-privileged process clears S_ISUID and
	 * S_ISGID. Symlinks are exempt (they have no executable
	 * payload); UFS guards similarly. PRIV_VFS_RETAINSUGID lets a
	 * privileged caller keep them. */
	if ((seen_uid || seen_gid) && (ino.mode & 06000) != 0 &&
	    vp->v_type != VLNK &&
	    cred != NULL &&
	    priv_check_cred(cred, PRIV_VFS_RETAINSUGID) != 0) {
		ino.mode &= ~06000;
	}
	if (seen_atime || seen_mtime || seen_mode || seen_uid || seen_gid) {
		struct timeval tv;
		getmicrotime(&tv);
		ino.ctime_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
		    (uint64_t)tv.tv_usec * 1000ULL;
		if (tessera_fs_inode_put(tmp_, (uint32_t)tn->inode_no,
		    &ino) != TESSERA_OK)
			return (EIO);
	}

	if (did_resize)
		vnode_pager_setsize(ap->a_vp, ino.size);
	tessera_fs_mark_dirty(tmp_);
	return (0);
}

static int
tessera_vop_open(struct vop_open_args *ap)
{
	struct vnode *vp = ap->a_vp;
	if (vp->v_type == VREG)
		(void)vnode_create_vobject(vp, 0, ap->a_td);
	return (0);
}

/*
 * vop_getpages — fill VM pages from file content for mmap / exec.
 *
 * The default vop_stdgetpages routes through vnode_pager_generic_getpages
 * which uses the buffer cache + VOP_STRATEGY. Tessera's pack model
 * doesn't fit a "logical block → physical block" mapping (chunks live in
 * content-addressed packs reached via two btrees), so we fill pages
 * directly via tessera_fs_read_inode_uio. One sf_buf-mapped page at a
 * time — short-term mappings, sleepable allocator, no recursion through
 * the strategy path that panics in bufstrategy().
 */
static int
tessera_vop_getpages(struct vop_getpages_args *ap)
{
	struct vnode *vp = ap->a_vp;
	vm_page_t   *ma  = ap->a_m;
	int          count = ap->a_count;

	if (vp->v_type != VREG) return (VM_PAGER_FAIL);

	struct tessera_node  *tn   = VTOTNODE(vp);
	struct tessera_mount *tmp_ = VFSTOTESSERA(vp->v_mount);
	if (tmp_->inode_tree == NULL) return (VM_PAGER_FAIL);

	/* mmap reads come from on-disk content via fetch_blob — drain
	 * any coalesced INLINE writes first so the bytes match what
	 * vop_read would return. */
	if (tn->snapshot_gen == 0)
		(void)tessera_fs_dirty_content_drain_one(tmp_,
		    (uint32_t)tn->inode_no);

	tessera_inode_record_t ino;
	int igrc = (tn->snapshot_gen != 0)
	    ? tessera_fs_inode_get_at_gen(tmp_, (uint32_t)tn->inode_no,
	          tn->snapshot_gen, &ino)
	    : tessera_fs_inode_get(tmp_, (uint32_t)tn->inode_no, &ino);
	if (igrc != TESSERA_OK) return (VM_PAGER_FAIL);

	int rv = VM_PAGER_OK;
	for (int i = 0; i < count; i++) {
		vm_page_t pg = ma[i];
		off_t     off = (off_t)pg->pindex << PAGE_SHIFT;

		struct sf_buf *sf = sf_buf_alloc(pg, 0);
		if (sf == NULL) { rv = VM_PAGER_FAIL; break; }
		void *kva = (void *)sf_buf_kva(sf);
		bzero(kva, PAGE_SIZE);

		if ((uint64_t)off < ino.size) {
			size_t to_read = (off + PAGE_SIZE > (off_t)ino.size)
			    ? (size_t)(ino.size - off)
			    : PAGE_SIZE;

			struct iovec iov = { .iov_base = kva,
			                     .iov_len  = to_read };
			struct uio uio;
			uio.uio_iov     = &iov;
			uio.uio_iovcnt  = 1;
			uio.uio_offset  = off;
			uio.uio_resid   = (ssize_t)to_read;
			uio.uio_segflg  = UIO_SYSSPACE;
			uio.uio_rw      = UIO_READ;
			uio.uio_td      = curthread;

			int err = tessera_fs_read_inode_uio(tmp_, &ino, &uio);
			if (err != 0) {
				sf_buf_free(sf);
				rv = VM_PAGER_FAIL;
				break;
			}
		}
		vm_page_valid(pg);
		sf_buf_free(sf);
	}

	if (ap->a_rbehind != NULL) *ap->a_rbehind = 0;
	if (ap->a_rahead  != NULL) *ap->a_rahead  = 0;
	return (rv);
}

/*
 * vop_putpages — write dirty VM pages back to the file.
 *
 * Don't go through VOP_WRITE: it calls vnode_pager_setsize on every
 * write, which mutates the same VM object whose lock vnode_pager_putpages
 * is holding around our call. Recursing through that path corrupts the
 * VM object's page list and the next sf_buf_free/page-touch faults.
 * NFS's vop_putpages takes the same shape — they call ncl_writerpc
 * directly rather than VOP_WRITE.
 *
 * Approach: read the existing file content into a kernel buffer, splice
 * in the dirty pages at their respective offsets, route the result
 * through tessera_fs_replace_content / replace_content_chunked
 * directly. One republish per putpages call regardless of page count
 * — tessera's content-addressed model rewrites the whole file anyway,
 * so per-page granularity wouldn't save anything.
 *
 * Lock dance: vnode_pager_putpages drops VM_OBJECT_WLOCK before calling
 * us and re-acquires after, so we don't touch it. Vnode lock is held
 * by the caller in some paths and not in others; replace_content
 * doesn't require it.
 */
static int
tessera_vop_putpages(struct vop_putpages_args *ap)
{
	struct vnode *vp     = ap->a_vp;
	vm_page_t   *ma      = ap->a_m;
	/* a_count is in BYTES (vnode_pager_putpages passes bytes; this
	 * differs from a_count in vop_getpages which passes pages). */
	int          npages  = ap->a_count / PAGE_SIZE;
	int         *rtvals  = ap->a_rtvals;

	if (vp->v_type != VREG || npages <= 0) {
		for (int i = 0; i < npages; i++) rtvals[i] = VM_PAGER_FAIL;
		return (VM_PAGER_FAIL);
	}

	struct tessera_node *tn = VTOTNODE(vp);
	struct tessera_mount *tmp_ = VFSTOTESSERA(vp->v_mount);
	if (tmp_->inode_tree == NULL) {
		for (int i = 0; i < npages; i++) rtvals[i] = VM_PAGER_FAIL;
		return (VM_PAGER_FAIL);
	}

	/* mmap-write to disk goes through replace_content_chunked below;
	 * drop any stale INLINE buffer first. */
	(void)tessera_fs_dirty_content_drain_one(tmp_, (uint32_t)tn->inode_no);

	tessera_inode_record_t ino;
	if (tessera_fs_inode_get(tmp_, (uint32_t)tn->inode_no, &ino)
	    != TESSERA_OK) {
		for (int i = 0; i < npages; i++) rtvals[i] = VM_PAGER_FAIL;
		return (VM_PAGER_FAIL);
	}

	off_t  base_off  = (off_t)ma[0]->pindex << PAGE_SHIFT;
	size_t pages_len = (size_t)npages * PAGE_SIZE;
	size_t new_size  = (size_t)base_off + pages_len;
	if ((uint64_t)new_size < ino.size) new_size = (size_t)ino.size;

	if (new_size > (64u * 1024u * 1024u)) {
		/* Match TESSERA_WRITE_MAX_BYTES — defined later in file. */
		for (int i = 0; i < npages; i++) rtvals[i] = VM_PAGER_FAIL;
		return (VM_PAGER_FAIL);
	}

	uint8_t *full = malloc(new_size, M_TESSERA, M_WAITOK | M_ZERO);

	if (ino.size > 0) {
		uint8_t *old_buf = NULL;
		size_t   old_len = 0;
		if (tessera_fs_read_full_content(tmp_, &ino, &old_buf,
		    &old_len) == 0 && old_buf != NULL) {
			size_t n = old_len < new_size ? old_len : new_size;
			memcpy(full, old_buf, n);
			free(old_buf, M_TESSERA);
		}
	}

	for (int i = 0; i < npages; i++) {
		struct sf_buf *sf = sf_buf_alloc(ma[i], 0);
		if (sf == NULL) {
			rtvals[i] = VM_PAGER_FAIL;
			continue;
		}
		size_t poff = (size_t)base_off + (size_t)i * PAGE_SIZE;
		memcpy(full + poff, (void *)sf_buf_kva(sf), PAGE_SIZE);
		sf_buf_free(sf);
		rtvals[i] = VM_PAGER_OK;
	}

	int rc;
	if (new_size <= (256u * 1024u))
		/* TESSERA_INLINE_THRESHOLD; defined later in file. */
		rc = tessera_fs_replace_content(tmp_,
		    (uint32_t)tn->inode_no, full, new_size);
	else
		rc = tessera_fs_replace_content_chunked(tmp_,
		    (uint32_t)tn->inode_no, full, new_size);

	free(full, M_TESSERA);

	if (rc != 0) {
		for (int i = 0; i < npages; i++) rtvals[i] = VM_PAGER_FAIL;
		return (VM_PAGER_FAIL);
	}
	tessera_fs_mark_dirty(tmp_);
	return (VM_PAGER_OK);
}

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
	struct tessera_node *tn = VTOTNODE(ap->a_vp);
	/* Drain this inode's coalesced writes — must publish before
	 * the SB-commit flush below makes the manifest hashes durable. */
	if (tn != NULL)
		(void)tessera_fs_dirty_content_drain_one(tmp_,
		    (uint32_t)tn->inode_no);
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
	uint64_t snapshots_root;
	uint64_t snapshots_gen;
	uint64_t next_inode_no;
	uint64_t meta_reserve_bump;
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
			tessera_metatrace(TM_OP_SNAPSHOT_REC, 0,
			    srec.generation, 1);
		}
		/* btree_put failure is non-fatal — losing one snapshot
		 * record doesn't break the live mount; future commits
		 * will retry. */

		/* Slice-4 retention: drop the oldest snapshot record once
		 * the retained count exceeds the configured horizon. The
		 * snapshots tree is keyed by 8-byte big-endian gen, so
		 * `seek_first` returns the lowest gen — that's the oldest.
		 *
		 * We do at most ONE drop per commit_sb. Over many commits,
		 * the count converges to the horizon. Doing more than one
		 * per commit would cascade COWs through the snapshots_tree
		 * and burn through meta-reserve.
		 *
		 * Mid-session un-pinning of the dropped snapshot's tree
		 * sectors is NOT done here — those sectors stay pinned in
		 * meta_pending until next mount's orphan-reclaim rebuilds
		 * the bitmap from the surviving snapshot set. Documented
		 * trade-off: meta_pending doesn't shrink mid-session, but
		 * the algorithm avoids a per-retirement multi-tree walk
		 * (which is exactly the failure mode that hung the VM in
		 * earlier slice-4 attempts).
		 *
		 * Runs at every commit, including unmount-flush — workloads
		 * that only commit at umount (the deferred-commit common
		 * case) need retention to fire there too, otherwise
		 * snapshots_gen grows unboundedly mount-to-mount.
		 */
		if (tessera_snapshot_retention > 0 &&
		    tmp_->sb.snapshots_gen >
		        (uint64_t)tessera_snapshot_retention) {
			tessera_btree_cursor_t *sc =
			    tessera_btree_seek_first(tmp_->snapshots_tree);
			if (sc != NULL) {
				uint8_t okey[8];
				tessera_snapshot_record_t orec;
				if (tessera_btree_cursor_get(sc, okey, &orec)
				    == TESSERA_OK) {
					tessera_btree_cursor_free(sc);
					sc = NULL;
					uint64_t after_root =
					    tmp_->sb.snapshots_root;
					if (tessera_btree_delete(
					    tmp_->snapshots_tree, okey,
					    &after_root) == TESSERA_OK) {
						tmp_->sb.snapshots_root =
						    after_root;
						tmp_->sb.snapshots_gen--;
						tessera_stat_snapshots_retired++;
						tessera_metatrace(
						    TM_OP_SNAPSHOT_REC,
						    1 /* retire marker */,
						    orec.generation, 0);
						/* The retired snapshot's
						 * tree sectors are no longer
						 * pinned by anything (unless
						 * shared via COW with a
						 * surviving snapshot). Rebuild
						 * the bitmap from the new
						 * retained set so the next
						 * meta_pending drain releases
						 * sectors that just lost
						 * their last referent.
						 * Without this rebuild,
						 * sustained-write workloads
						 * exhaust meta-reserve in a
						 * few hundred commits (bug #2). */
						tessera_meta_pin_bitmap_rebuild(
						    tmp_);
					}
				} else {
					tessera_btree_cursor_free(sc);
				}
			}
		}
	}

	/* Barrier #1: ensure all prior pack/btree/manifest writes are
	 * durable on the host file BEFORE we write the journal record
	 * that names them as committed. Without this, a crash with the
	 * journal record on disk but the data sectors still in qemu RAM
	 * would have replay re-applying a record whose payload references
	 * sectors that don't have the right contents. (No-op fast path
	 * if cp is NULL.) */
	tessera_kbio_barrier(&tmp_->bio_ctx);

	if (tmp_->journal != NULL) {
		uint64_t tx;
		if (tessera_journal_tx_begin(tmp_->journal, &tx,
		    "sb_commit") == TESSERA_OK) {
			struct tessera_jrec_sb_commit body;
			body.generation         = tmp_->sb.generation;
			body.inode_root         = tmp_->sb.inode_root;
			body.pack_registry_root = tmp_->sb.pack_registry_root;
			body.free_extent_root   = tmp_->sb.free_extent_root;
			body.snapshots_root     = tmp_->sb.snapshots_root;
			body.snapshots_gen      = tmp_->sb.snapshots_gen;
			body.next_inode_no      = tmp_->sb.next_inode_no;
			body.meta_reserve_bump  = tmp_->sb.meta_reserve_bump;
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

	/* Barrier #2: ensure SB-A and SB-B are both durable on the host
	 * file BEFORE journal_checkpoint advances head=tail=1, retiring
	 * the records that would let replay roll forward across a torn
	 * SB write. Without this, a crash between the SB write (in qemu
	 * RAM) and the checkpoint write (also in qemu RAM, but it's the
	 * later sector so qemu may flush it first) leaves disk with an
	 * empty journal AND a stale on-disk SB — unrecoverable state. */
	tessera_kbio_barrier(&tmp_->bio_ctx);

	/* SB durably advanced. The journal record we just appended is now
	 * applied; checkpoint frees its sectors so the next commit doesn't
	 * push us toward the journal-full wrap. Crash between the SB write
	 * and the checkpoint is harmless: replay-on-mount checks
	 * `record.gen > sb.gen` so the already-applied record is skipped. */
	if (tmp_->journal != NULL)
		(void)tessera_journal_checkpoint(tmp_->journal);

	/* The new SB is durable, so the OLD SB no longer references any
	 * of the meta-reserve sectors freed during this commit cycle.
	 * Decide what to do with each pending sector:
	 *
	 *   - If meta_pin_bitmap[s] is SET → some retained snapshot still
	 *     references it. KEEP pinned in meta_pending so it doesn't
	 *     get reused (would corrupt forensic mounts of that snapshot).
	 *   - Otherwise → push to meta_free for reuse on the next
	 *     meta_alloc.
	 *
	 * v2 slice-4 fix: this is the gate that stops snapshot corruption.
	 * The bitmap was built once at mount time by walking every retained
	 * snapshot's btree (mountfs ~line 1067 below the kbio init). Pinned
	 * sectors NEVER leave meta_pending until unmount; the next mount's
	 * orphan-recycler will rebuild the bitmap and decide afresh — at
	 * which point any newly-aged-out snapshots' sectors get released.
	 * That's the contract: retention happens at mount, not mid-session.
	 */
	tessera_metatrace(TM_OP_DRAIN_BEGIN, 0, tmp_->sb.generation,
	    tmp_->meta_pending_count);
	if (tmp_->meta_pending_count > 0 && tmp_->meta_free != NULL) {
		const uint64_t mstart = tmp_->sb.meta_reserve_start;
		const uint64_t mlen   = tmp_->sb.meta_reserve_length;
		uint32_t kept = 0;
		for (uint32_t i = 0; i < tmp_->meta_pending_count; i++) {
			uint64_t s = tmp_->meta_pending[i];
			int pinned = 0;
			if (tmp_->meta_pin_bitmap != NULL &&
			    s >= mstart && s < mstart + mlen) {
				uint64_t bit = s - mstart;
				if (tmp_->meta_pin_bitmap[bit / 8]
				    & (1u << (bit % 8))) {
					pinned = 1;
				}
			}
			if (pinned) {
				tmp_->meta_pending[kept++] = s;
				tessera_metatrace(TM_OP_DRAIN_KEEP, s,
				    tmp_->sb.generation, kept);
			} else if (tmp_->meta_free_count
			    < tmp_->meta_free_cap) {
				tmp_->meta_free[tmp_->meta_free_count++] = s;
				tessera_metatrace(TM_OP_DRAIN_RELEASE, s,
				    tmp_->sb.generation,
				    tmp_->meta_free_count);
			}
		}
		tmp_->meta_pending_count = kept;
	}
	tessera_metatrace(TM_OP_DRAIN_END, 0, tmp_->sb.generation,
	    tmp_->meta_pending_count);
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
	 * Pressure triggers — force flush when:
	 *   1. meta-reserve usage > 50% (was 75% before step-2b's
	 *      dirty-inode batching; tightened so drain has headroom
	 *      for its own btree_puts).
	 *   2. dirty_count > 64 — bounds per-flush drain work and
	 *      meta-reserve burst at drain time. Keeps the
	 *      200-create-into-one-dir workload from accumulating
	 *      a full meta-reserve's worth of deferred btree_puts.
	 */
	if (tmp_->dirty_init && tmp_->dirty_count > 64u) {
		(void)tessera_fs_flush(tmp_);
		return;
	}
	/* v2.6: dirent log overflow trigger. */
	if (tmp_->dirty_init &&
	    tmp_->dirent_log_count >
	        (uint32_t)tessera_dirent_log_threshold) {
		(void)tessera_fs_flush(tmp_);
		return;
	}
	/* Cap pending-manifest cache size — past 1 MiB held in RAM,
	 * force a drain. */
	if (tmp_->dirty_init && tmp_->pending_manifest_bytes >=
	    TESSERA_PENDING_MANIFEST_BYTES_MAX) {
		(void)tessera_fs_flush(tmp_);
		return;
	}
	const uint64_t used = tmp_->sb.meta_reserve_bump
	    - tmp_->sb.meta_reserve_start;
	const uint64_t cap  = tmp_->sb.meta_reserve_length;
	const uint64_t free = (uint64_t)tmp_->meta_free_count;
	if (cap > 0 && (used > free) &&
	    (used - free) * 2 >= cap) {
		(void)tessera_fs_flush(tmp_);
	}

	/* C2 — background repack trigger. Cheap check on every dirty
	 * mutation: if MULTI_EXTENT-flagged pack count crosses the
	 * threshold, arm the background task. taskqueue_enqueue is a
	 * no-op when the task is already pending or running, so
	 * spamming it on every mutation is fine. */
	if (tmp_->repack_task_init && !tmp_->flush_unmounting &&
	    tmp_->multi_extent_pack_count >
	        (uint32_t)tessera_repack_threshold) {
		(void)taskqueue_enqueue(taskqueue_thread, &tmp_->repack_task);
	}
}

/* v2 step-2b: dirty-inode write-back. inode_get/put/delete go through
 * the in-memory cache; reads consult dirty_inodes first, falling back
 * to btree_get; writes stage there until flush. */
static int
tessera_fs_inode_get(struct tessera_mount *tmp_, uint32_t inode_no,
                     tessera_inode_record_t *out)
{
	int rc;
	if (tmp_->dirty_init) {
		mtx_lock(&tmp_->flush_mtx);
		uint32_t b = inode_no & (TESSERA_DIRTY_INODE_BUCKETS - 1u);
		struct tessera_dirty_inode *e;
		int found = 0;
		LIST_FOREACH(e, &tmp_->dirty_inodes[b], link) {
			if (e->inode_no == inode_no) {
				if (e->tombstone) {
					mtx_unlock(&tmp_->flush_mtx);
					return (TESSERA_ENOENT);
				}
				memcpy(out, &e->rec, sizeof *out);
				found = 1;
				break;
			}
		}
		if (found) {
			/* While the lock is held, also overlay the live size
			 * from any dirty_content buffer. vop_write doesn't
			 * patch ino.size on every call (would be a btree
			 * write per write); instead, the buffer's `size`
			 * field is the live file size until the buffer
			 * publishes. */
			uint32_t cb = inode_no &
			    (TESSERA_DIRTY_CONTENT_BUCKETS - 1u);
			struct tessera_dirty_content *dc;
			LIST_FOREACH(dc, &tmp_->dirty_content[cb], link) {
				if (dc->inode_no == inode_no) {
					if ((uint64_t)dc->size > out->size)
						out->size = dc->size;
					break;
				}
			}
			mtx_unlock(&tmp_->flush_mtx);
			return (TESSERA_OK);
		}
		mtx_unlock(&tmp_->flush_mtx);
	}

	if (tmp_->inode_tree == NULL) return (TESSERA_ENOENT);
	uint8_t key[4];
	encode_inode_key(inode_no, key);
	rc = tessera_btree_get(tmp_->inode_tree, key, out);
	if (rc != TESSERA_OK) return (rc);

	/* Overlay live size from dirty_content for the (rarer) case
	 * where the inode wasn't in the dirty_inodes cache. */
	if (tmp_->dirty_init) {
		mtx_lock(&tmp_->flush_mtx);
		uint32_t cb = inode_no & (TESSERA_DIRTY_CONTENT_BUCKETS - 1u);
		struct tessera_dirty_content *dc;
		LIST_FOREACH(dc, &tmp_->dirty_content[cb], link) {
			if (dc->inode_no == inode_no) {
				if ((uint64_t)dc->size > out->size)
					out->size = dc->size;
				break;
			}
		}
		mtx_unlock(&tmp_->flush_mtx);
	}
	return (TESSERA_OK);
}

static uint32_t
_inode_key_decode(const uint8_t key[4])
{
	return ((uint32_t)key[0] << 24) | ((uint32_t)key[1] << 16) |
	    ((uint32_t)key[2] << 8) | (uint32_t)key[3];
}

static int
tessera_fs_inode_get_byk(struct tessera_mount *tmp_, const uint8_t key[4],
                         tessera_inode_record_t *out)
{
	return (tessera_fs_inode_get(tmp_, _inode_key_decode(key), out));
}

/*
 * v2 slice-3: helper to reject mutations targeting magic-dir or
 * snapshot vnodes. Returns EROFS when the vnode is read-only by
 * virtue of being inside `/.tessera/snapshots/...`.
 */
static inline int
tessera_node_is_readonly(struct tessera_node *tn)
{
	return (tn->kind != TESSERA_NODE_REGULAR || tn->snapshot_gen != 0);
}

/*
 * v2 slice-3: read an inode record from a specific snapshot's
 * inode_tree (instead of the live one). Used by the magic-dir
 * read paths so vnodes inside `/.tessera/snapshots/<gen>/...`
 * see the historical state.
 *
 * Strategy: look up the snapshot record for `gen` to learn its
 * inode_root, then open a fresh btree handle against that root
 * for this single get. Open/close per access is cheap relative
 * to the actual sector reads; a per-mount cache could be added
 * later if profiling shows it matters.
 *
 * Returns TESSERA_OK on success, TESSERA_ENOENT if the snapshot
 * doesn't exist or the inode isn't in it.
 */
static int
tessera_fs_inode_get_at_gen(struct tessera_mount *tmp_, uint32_t inode_no,
                            uint64_t snapshot_gen,
                            tessera_inode_record_t *out)
{
	if (snapshot_gen == 0)
		return (tessera_fs_inode_get(tmp_, inode_no, out));
	if (tmp_->snapshots_tree == NULL)
		return (TESSERA_ENOENT);

	uint8_t skey[8];
	for (int i = 0; i < 8; i++)
		skey[i] = (uint8_t)(snapshot_gen >> ((7 - i) * 8));
	tessera_snapshot_record_t srec;
	if (tessera_btree_get(tmp_->snapshots_tree, skey, &srec) != TESSERA_OK)
		return (TESSERA_ENOENT);
	if (srec.inode_root == 0)
		return (TESSERA_ENOENT);

	tessera_btree_t *t = tessera_btree_open(&tmp_->meta_bio,
	    srec.inode_root, /*tree_kind*/ 0,
	    /*key*/ 4, /*value*/ TESSERA_INODE_RECORD_SIZE);
	if (t == NULL) return (TESSERA_ENOENT);

	uint8_t key[4];
	encode_inode_key(inode_no, key);
	int rc = tessera_btree_get(t, key, out);
	tessera_btree_close(t);
	return (rc);
}

static int
tessera_fs_inode_put_byk(struct tessera_mount *tmp_, const uint8_t key[4],
                         const tessera_inode_record_t *rec,
                         uint64_t *out_root)
{
	int r = tessera_fs_inode_put(tmp_, _inode_key_decode(key), rec);
	if (r == TESSERA_OK && out_root != NULL)
		*out_root = tmp_->sb.inode_root;
	return (r);
}

static int
tessera_fs_inode_delete_byk(struct tessera_mount *tmp_, const uint8_t key[4],
                            uint64_t *out_root)
{
	int r = tessera_fs_inode_delete(tmp_, _inode_key_decode(key));
	if (r == TESSERA_OK && out_root != NULL)
		*out_root = tmp_->sb.inode_root;
	return (r);
}

static int
tessera_fs_inode_put(struct tessera_mount *tmp_, uint32_t inode_no,
                     const tessera_inode_record_t *rec)
{
	if (!tmp_->dirty_init) {
		/* Pre-init mount-time path — write through directly. */
		if (tmp_->inode_tree == NULL) return (TESSERA_EIO);
		uint8_t key[4];
		encode_inode_key(inode_no, key);
		uint64_t new_root = tmp_->sb.inode_root;
		int r = tessera_btree_put(tmp_->inode_tree, key, rec,
		    &new_root);
		if (r == TESSERA_OK) tmp_->sb.inode_root = new_root;
		return (r);
	}

	/* B.2 — clone for the journal pending queue BEFORE we take
	 * flush_mtx; malloc(M_WAITOK) is forbidden under it. */
	struct tessera_pending_inode *jc = NULL;
	if (tessera_journal_log_enable_default && tmp_->journal != NULL && !tmp_->in_replay) {
		jc = malloc(sizeof *jc, M_TESSERA, M_WAITOK | M_ZERO);
		jc->inode_no  = inode_no;
		jc->tombstone = 0;
		memcpy(&jc->rec, rec, sizeof jc->rec);
	}

	/* Coalescing: enqueue the pre-built jc; if a pending entry
	 * already exists for this inode_no, supersede its body
	 * in-place and free jc. Caller holds flush_mtx. */
	int dropped_jc = 0;

	mtx_lock(&tmp_->flush_mtx);
	uint32_t b = inode_no & (TESSERA_DIRTY_INODE_BUCKETS - 1u);
	struct tessera_dirty_inode *e;
	int found = 0;
	LIST_FOREACH(e, &tmp_->dirty_inodes[b], link) {
		if (e->inode_no == inode_no) {
			memcpy(&e->rec, rec, sizeof e->rec);
			e->tombstone = 0;
			found = 1;
			break;
		}
	}
	if (!found) {
		mtx_unlock(&tmp_->flush_mtx);
		/* malloc(M_WAITOK) outside the mutex. */
		e = malloc(sizeof *e, M_TESSERA, M_WAITOK | M_ZERO);
		e->inode_no = inode_no;
		memcpy(&e->rec, rec, sizeof e->rec);
		mtx_lock(&tmp_->flush_mtx);
		struct tessera_dirty_inode *existing;
		int raced = 0;
		LIST_FOREACH(existing, &tmp_->dirty_inodes[b], link) {
			if (existing->inode_no == inode_no) {
				memcpy(&existing->rec, rec, sizeof existing->rec);
				existing->tombstone = 0;
				raced = 1; break;
			}
		}
		if (raced) {
			free(e, M_TESSERA);
		} else {
			LIST_INSERT_HEAD(&tmp_->dirty_inodes[b], e, link);
			tmp_->dirty_count++;
		}
	}
	if (jc != NULL) {
		struct tessera_pending_inode *pi;
		LIST_FOREACH(pi, &tmp_->journal_pending_inodes, link) {
			if (pi->inode_no == jc->inode_no) {
				pi->tombstone = jc->tombstone;
				memcpy(&pi->rec, &jc->rec, sizeof pi->rec);
				dropped_jc = 1;
				break;
			}
		}
		if (!dropped_jc) {
			LIST_INSERT_HEAD(&tmp_->journal_pending_inodes, jc,
			    link);
			tmp_->journal_pending_inode_count++;
		}
	}
	mtx_unlock(&tmp_->flush_mtx);
	if (jc != NULL && dropped_jc) free(jc, M_TESSERA);
	return (TESSERA_OK);
}

static int
tessera_fs_inode_delete(struct tessera_mount *tmp_, uint32_t inode_no)
{
	/* Discard any coalesced writes — file is gone. */
	tessera_fs_dirty_content_drop(tmp_, inode_no);
	if (!tmp_->dirty_init) {
		if (tmp_->inode_tree == NULL) return (TESSERA_EIO);
		uint8_t key[4];
		encode_inode_key(inode_no, key);
		uint64_t new_root = tmp_->sb.inode_root;
		int r = tessera_btree_delete(tmp_->inode_tree, key,
		    &new_root);
		if (r == TESSERA_OK) tmp_->sb.inode_root = new_root;
		return (r);
	}

	struct tessera_pending_inode *jc = NULL;
	if (tessera_journal_log_enable_default && tmp_->journal != NULL && !tmp_->in_replay) {
		jc = malloc(sizeof *jc, M_TESSERA, M_WAITOK | M_ZERO);
		jc->inode_no  = inode_no;
		jc->tombstone = 1;
	}

	int dropped_jc = 0;
	mtx_lock(&tmp_->flush_mtx);
	uint32_t b = inode_no & (TESSERA_DIRTY_INODE_BUCKETS - 1u);
	struct tessera_dirty_inode *e;
	int found = 0;
	LIST_FOREACH(e, &tmp_->dirty_inodes[b], link) {
		if (e->inode_no == inode_no) {
			e->tombstone = 1;
			found = 1; break;
		}
	}
	if (!found) {
		mtx_unlock(&tmp_->flush_mtx);
		e = malloc(sizeof *e, M_TESSERA, M_WAITOK | M_ZERO);
		e->inode_no  = inode_no;
		e->tombstone = 1;
		mtx_lock(&tmp_->flush_mtx);
		struct tessera_dirty_inode *existing;
		int raced = 0;
		LIST_FOREACH(existing, &tmp_->dirty_inodes[b], link) {
			if (existing->inode_no == inode_no) {
				existing->tombstone = 1;
				raced = 1; break;
			}
		}
		if (raced) {
			free(e, M_TESSERA);
		} else {
			LIST_INSERT_HEAD(&tmp_->dirty_inodes[b], e, link);
			tmp_->dirty_count++;
		}
	}
	if (jc != NULL) {
		struct tessera_pending_inode *pi;
		LIST_FOREACH(pi, &tmp_->journal_pending_inodes, link) {
			if (pi->inode_no == jc->inode_no) {
				pi->tombstone = 1;
				dropped_jc = 1; break;
			}
		}
		if (!dropped_jc) {
			LIST_INSERT_HEAD(&tmp_->journal_pending_inodes, jc,
			    link);
			tmp_->journal_pending_inode_count++;
		}
	}
	mtx_unlock(&tmp_->flush_mtx);
	if (jc != NULL && dropped_jc) free(jc, M_TESSERA);
	return (TESSERA_OK);
}

/* Drain just the tombstone entries from dirty_inodes. Done as a
 * pre-pass in tessera_fs_flush so that subsequent gc_data_zone()
 * sees the to-be-deleted inodes as gone — their content packs
 * (CHUNK_LIST blobs etc.) become reclaimable orphans BEFORE the
 * pending_manifests_drain that needs the freed sectors.
 *
 * Without this pre-pass, an rm-heavy workload churns dir-manifest
 * packs while the just-deleted files' content packs sit in registry
 * waiting for next mount-time GC. The data zone fills to 100% and
 * subsequent publishes ENOSPC even though megabytes are logically
 * free. Surfaced via stress_exhaustion.sh.
 *
 * Tombstones only need meta-reserve space (btree_delete on inode_tree),
 * not data-zone space, so they can drain even when the data zone is
 * tight. Non-tombstone entries stay in dirty_inodes for the regular
 * drain. */
static int
tessera_fs_dirty_inodes_drain_tombstones(struct tessera_mount *tmp_)
{
	if (!tmp_->dirty_init || tmp_->inode_tree == NULL) return (0);

	LIST_HEAD(, tessera_dirty_inode) snap;
	LIST_INIT(&snap);
	mtx_lock(&tmp_->flush_mtx);
	uint32_t snap_count = 0;
	for (uint32_t b = 0; b < TESSERA_DIRTY_INODE_BUCKETS; b++) {
		struct tessera_dirty_inode *e, *next;
		LIST_FOREACH_SAFE(e, &tmp_->dirty_inodes[b], link, next) {
			if (!e->tombstone) continue;
			LIST_REMOVE(e, link);
			LIST_INSERT_HEAD(&snap, e, link);
			snap_count++;
			tmp_->dirty_count--;
		}
	}
	mtx_unlock(&tmp_->flush_mtx);
	if (snap_count == 0) return (0);

	int err = 0;
	uint64_t root = tmp_->sb.inode_root;
	struct tessera_dirty_inode *e;
	while ((e = LIST_FIRST(&snap)) != NULL) {
		LIST_REMOVE(e, link);
		uint8_t key[4];
		encode_inode_key(e->inode_no, key);
		int r = tessera_btree_delete(tmp_->inode_tree, key, &root);
		if (r != TESSERA_OK && r != TESSERA_ENOENT) {
			err = EIO;
			/* Restore on failure so the regular drain retries. */
			mtx_lock(&tmp_->flush_mtx);
			uint32_t b = e->inode_no &
			    (TESSERA_DIRTY_INODE_BUCKETS - 1u);
			LIST_INSERT_HEAD(&tmp_->dirty_inodes[b], e, link);
			tmp_->dirty_count++;
			mtx_unlock(&tmp_->flush_mtx);
			break;
		}
		free(e, M_TESSERA);
	}
	while ((e = LIST_FIRST(&snap)) != NULL) {
		LIST_REMOVE(e, link);
		mtx_lock(&tmp_->flush_mtx);
		uint32_t b = e->inode_no & (TESSERA_DIRTY_INODE_BUCKETS - 1u);
		LIST_INSERT_HEAD(&tmp_->dirty_inodes[b], e, link);
		tmp_->dirty_count++;
		mtx_unlock(&tmp_->flush_mtx);
	}
	if (err == 0) tmp_->sb.inode_root = root;
	return (err);
}

/* Drain dirty_inodes → inode_tree. Two-phase: snapshot the entire
 * dirty set into a local list under the mutex, then process without
 * holding it (btree_put may sleep on memory alloc; witness will
 * complain if we hold an mtx across that). New mutations that race
 * with the drain land in a fresh dirty_inodes — they're safe and
 * picked up by the next flush. */
static int
tessera_fs_dirty_inodes_drain(struct tessera_mount *tmp_)
{
	if (!tmp_->dirty_init) return (0);
	if (tmp_->inode_tree == NULL) return (TESSERA_EIO);

	LIST_HEAD(, tessera_dirty_inode) snap;
	LIST_INIT(&snap);

	mtx_lock(&tmp_->flush_mtx);
	uint32_t snap_count = 0;
	for (uint32_t b = 0; b < TESSERA_DIRTY_INODE_BUCKETS; b++) {
		struct tessera_dirty_inode *e;
		while ((e = LIST_FIRST(&tmp_->dirty_inodes[b])) != NULL) {
			LIST_REMOVE(e, link);
			LIST_INSERT_HEAD(&snap, e, link);
			snap_count++;
		}
	}
	tmp_->dirty_count = 0;
	mtx_unlock(&tmp_->flush_mtx);
	if (snap_count == 0) return (0);
	tessera_stat_dirty_drained += snap_count;

	/* Process without the lock. Failures partway leave residual
	 * entries leaked — better than corrupting the cache state.
	 * Successful path frees every entry. */
	int err = 0;
	uint64_t root = tmp_->sb.inode_root;
	struct tessera_dirty_inode *e;
	while ((e = LIST_FIRST(&snap)) != NULL) {
		LIST_REMOVE(e, link);
		uint8_t key[4];
		encode_inode_key(e->inode_no, key);
		int r;
		if (e->tombstone) {
			r = tessera_btree_delete(tmp_->inode_tree, key, &root);
			if (r != TESSERA_OK && r != TESSERA_ENOENT) {
				printf("tessera_fs: dirty_inodes_drain — "
				    "btree_delete inode_no=%u failed: r=%d "
				    "root=%llu (drained %u/%u)\n",
				    (unsigned)e->inode_no, r,
				    (unsigned long long)root,
				    (unsigned)tessera_stat_dirty_drained,
				    (unsigned)snap_count);
				err = EIO;
			}
		} else {
			r = tessera_btree_put(tmp_->inode_tree, key,
			    &e->rec, &root);
			if (r != TESSERA_OK) {
				printf("tessera_fs: dirty_inodes_drain — "
				    "btree_put inode_no=%u failed: r=%d "
				    "root=%llu (drained %u/%u)\n",
				    (unsigned)e->inode_no, r,
				    (unsigned long long)root,
				    (unsigned)tessera_stat_dirty_drained,
				    (unsigned)snap_count);
				err = EIO;
			}
		}
		if (err == 0) {
			free(e, M_TESSERA);
		} else {
			/* Restore the failed entry to the cache so the next
			 * flush can retry. Bug #3: prior code freed it,
			 * losing the inode update — subsequent ops saw the
			 * pre-update inode record from on-disk btree. */
			mtx_lock(&tmp_->flush_mtx);
			uint32_t b = e->inode_no &
			    (TESSERA_DIRTY_INODE_BUCKETS - 1u);
			LIST_INSERT_HEAD(&tmp_->dirty_inodes[b], e, link);
			tmp_->dirty_count++;
			mtx_unlock(&tmp_->flush_mtx);
			break;
		}
	}
	/* Restore any leftover entries (only on error) to the cache. */
	while ((e = LIST_FIRST(&snap)) != NULL) {
		LIST_REMOVE(e, link);
		mtx_lock(&tmp_->flush_mtx);
		uint32_t b = e->inode_no & (TESSERA_DIRTY_INODE_BUCKETS - 1u);
		LIST_INSERT_HEAD(&tmp_->dirty_inodes[b], e, link);
		tmp_->dirty_count++;
		mtx_unlock(&tmp_->flush_mtx);
	}
	if (err == 0) tmp_->sb.inode_root = root;
	return (err);
}

/* v2 step-2b: pending-manifest cache.
 *
 * Each entry holds (hash, bytes) for a manifest that was emitted by
 * publish_manifest but not yet packed onto disk. fetch_blob consults
 * the cache before scanning pack_registry, so subsequent reads of
 * the manifest during the same flush window come straight from RAM.
 *
 * Bucketing: hash[0] selects 1 of 256 buckets. Roughly uniform.
 *
 * Lock: flush_mtx (same one that protects dirty_inodes).
 */
static int
tessera_fs_pending_manifest_put(struct tessera_mount *tmp_,
                                const tessera_hash_t hash,
                                const uint8_t *bytes, uint32_t len,
                                uint32_t owner_inode_no)
{
	if (!tmp_->dirty_init) return (TESSERA_EIO);

	/* malloc outside the mtx (M_WAITOK can sleep). */
	uint8_t *buf = malloc(len, M_TESSERA, M_WAITOK);
	memcpy(buf, bytes, len);
	struct tessera_pending_manifest *e =
	    malloc(sizeof *e, M_TESSERA, M_WAITOK | M_ZERO);
	memcpy(e->hash, hash, sizeof e->hash);
	e->bytes = buf;
	e->len   = len;
	LIST_INIT(&e->owners);

	struct tessera_pending_owner *new_own = NULL;
	if (owner_inode_no != 0) {
		new_own = malloc(sizeof *new_own, M_TESSERA, M_WAITOK | M_ZERO);
		new_own->inode_no = owner_inode_no;
	}

	mtx_lock(&tmp_->flush_mtx);

	/* Supersession FIRST: if the new manifest is tagged with an
	 * owner_inode_no, walk every bucket and remove this owner from
	 * any older pending entry that listed it. An entry whose owner
	 * list becomes empty is unreferenced and gets freed. The shared-
	 * dedup case (four files with content "x\n" all owning the same
	 * pending manifest) used to fail here: the old code deleted the
	 * entry on the first owner's supersession, leaving the other
	 * three inodes pointing at a hash that fetch_blob couldn't find
	 * (not yet on disk, and no longer in the pending cache). */
	if (owner_inode_no != 0) {
		for (uint32_t bb = 0;
		    bb < TESSERA_PENDING_MANIFEST_BUCKETS; bb++) {
			struct tessera_pending_manifest *prev, *tmp_e;
			LIST_FOREACH_SAFE(prev,
			    &tmp_->pending_manifests[bb], link, tmp_e) {
				struct tessera_pending_owner *po, *po_n;
				int removed = 0;
				LIST_FOREACH_SAFE(po, &prev->owners, link, po_n) {
					if (po->inode_no == owner_inode_no) {
						LIST_REMOVE(po, link);
						free(po, M_TESSERA);
						removed = 1;
					}
				}
				/* Only retire the pending entry if we *just*
				 * emptied its owners list. Entries published
				 * with owner=0 (e.g. dir manifests via
				 * publish_manifest_owned_known_new at
				 * dir_btree_publish_*) have an EMPTY owners
				 * list to begin with — skipping them here
				 * was the bug that made truncate(file)
				 * silently wipe the parent directory. */
				if (removed && LIST_EMPTY(&prev->owners)) {
					LIST_REMOVE(prev, link);
					tmp_->pending_manifest_count--;
					tmp_->pending_manifest_bytes -=
					    prev->len;
					free(prev->bytes, M_TESSERA);
					free(prev, M_TESSERA);
				}
			}
		}
	}

	uint32_t b = hash[0];
	struct tessera_pending_manifest *existing;
	LIST_FOREACH(existing, &tmp_->pending_manifests[b], link) {
		if (memcmp(existing->hash, hash, TESSERA_HASH_SIZE) == 0) {
			/* Same hash already cached; identical bytes by
			 * construction. Drop the new copy but ADD the new
			 * owner to the existing entry so a later supersede
			 * by some OTHER owner doesn't yank the bytes out
			 * from under us. */
			if (new_own != NULL) {
				int already = 0;
				struct tessera_pending_owner *po;
				LIST_FOREACH(po, &existing->owners, link) {
					if (po->inode_no ==
					    new_own->inode_no) {
						already = 1; break;
					}
				}
				if (already) {
					free(new_own, M_TESSERA);
				} else {
					LIST_INSERT_HEAD(&existing->owners,
					    new_own, link);
				}
			}
			mtx_unlock(&tmp_->flush_mtx);
			free(buf, M_TESSERA);
			free(e, M_TESSERA);
			return (TESSERA_OK);
		}
	}

	if (new_own != NULL)
		LIST_INSERT_HEAD(&e->owners, new_own, link);
	LIST_INSERT_HEAD(&tmp_->pending_manifests[b], e, link);
	tmp_->pending_manifest_count++;
	tmp_->pending_manifest_bytes += len;
	mtx_unlock(&tmp_->flush_mtx);
	return (TESSERA_OK);
}

/* Returns 1 (and copies bytes into a freshly malloc'd buffer) on hit;
 * 0 on miss. Caller is responsible for freeing *out_bytes on hit. */
static int
tessera_fs_pending_manifest_lookup(struct tessera_mount *tmp_,
                                   const tessera_hash_t hash,
                                   uint8_t **out_bytes, uint32_t *out_len)
{
	if (!tmp_->dirty_init) return (0);
	mtx_lock(&tmp_->flush_mtx);
	uint32_t b = hash[0];
	struct tessera_pending_manifest *e;
	LIST_FOREACH(e, &tmp_->pending_manifests[b], link) {
		if (memcmp(e->hash, hash, TESSERA_HASH_SIZE) == 0) {
			uint32_t len = e->len;
			uint8_t *buf = malloc(len, M_TESSERA, M_NOWAIT);
			if (buf == NULL) {
				mtx_unlock(&tmp_->flush_mtx);
				return (0);
			}
			memcpy(buf, e->bytes, len);
			mtx_unlock(&tmp_->flush_mtx);
			*out_bytes = buf;
			*out_len   = len;
			return (1);
		}
	}
	mtx_unlock(&tmp_->flush_mtx);
	return (0);
}

/* Drain the pending-manifest cache: snapshot under lock, publish
 * each to disk without the lock, free the bytes. Identical structure
 * to dirty_inodes_drain. Called by tessera_fs_flush BEFORE
 * dirty_inodes_drain so by the time inode records hit btree their
 * manifest_hash references are real packs. */
static int
tessera_fs_pending_manifests_drain(struct tessera_mount *tmp_)
{
	if (!tmp_->dirty_init) return (0);

	LIST_HEAD(, tessera_pending_manifest) snap;
	LIST_INIT(&snap);

	mtx_lock(&tmp_->flush_mtx);
	uint32_t snap_count = 0;
	for (uint32_t b = 0; b < TESSERA_PENDING_MANIFEST_BUCKETS; b++) {
		struct tessera_pending_manifest *e;
		while ((e = LIST_FIRST(&tmp_->pending_manifests[b])) != NULL) {
			LIST_REMOVE(e, link);
			LIST_INSERT_HEAD(&snap, e, link);
			snap_count++;
		}
	}
	tmp_->pending_manifest_count = 0;
	tmp_->pending_manifest_bytes = 0;
	mtx_unlock(&tmp_->flush_mtx);
	if (snap_count == 0) return (0);
	tessera_stat_pending_drained += snap_count;

	int err = 0;

	/* Two-tier publishing: small eligible manifests get aggregated
	 * into multi-blob packs; large manifests fall back to the
	 * single-blob path so they don't get bloated by neighbours.
	 *
	 * The aggregation cuts per-pack header/index overhead from
	 * ~16 KiB per tiny file to ~1 KiB amortized. Critical for
	 * tessera-import workloads (3000-file app installs).
	 *
	 * Per-blob dedup: before adding to a batch we check the CAS
	 * cache; warm hashes mean the content is already on disk and
	 * we can skip publishing entirely (the existing pending entry
	 * is removed and freed without writing).
	 */
	struct tessera_pending_manifest *e;
	struct tessera_aggr_entry *batch =
	    malloc((size_t)tessera_aggregation_max_blobs *
	        sizeof *batch, M_TESSERA, M_WAITOK);
	struct tessera_pending_manifest **batch_owners =
	    malloc((size_t)tessera_aggregation_max_blobs *
	        sizeof *batch_owners, M_TESSERA, M_WAITOK);
	uint32_t bn = 0;
	size_t bbytes = 0;

	/* Helper: flush the current batch (publish + free its owners).
	 * Defined inline to share state. */
#define FLUSH_BATCH() do { \
	if (bn > 0 && err == 0) { \
		int _r = tessera_fs_publish_manifests_batch(tmp_, \
		    batch, bn); \
		if (_r != 0) err = _r; \
	} \
	for (uint32_t _i = 0; _i < bn; _i++) { \
		struct tessera_pending_manifest *_pm = batch_owners[_i]; \
		if (err == 0) { \
			struct tessera_pending_owner *_po; \
			while ((_po = LIST_FIRST(&_pm->owners)) != NULL) { \
				LIST_REMOVE(_po, link); \
				free(_po, M_TESSERA); \
			} \
			free(_pm->bytes, M_TESSERA); \
			free(_pm, M_TESSERA); \
		} else { \
			mtx_lock(&tmp_->flush_mtx); \
			uint32_t _bk = _pm->hash[0]; \
			LIST_INSERT_HEAD(&tmp_->pending_manifests[_bk], \
			    _pm, link); \
			tmp_->pending_manifest_count++; \
			tmp_->pending_manifest_bytes += _pm->len; \
			mtx_unlock(&tmp_->flush_mtx); \
		} \
	} \
	bn = 0; \
	bbytes = 0; \
} while (0)

	while ((e = LIST_FIRST(&snap)) != NULL) {
		LIST_REMOVE(e, link);

		/* Per-blob dedup: if we've already got a pack containing
		 * this blob hash on disk (or in the warm CAS cache),
		 * skip publishing — the existing copy is canonical. */
		struct tessera_cas_loc_snap snap_dummy;
		if (tessera_cas_loc_lookup(&tmp_->cas_cache, e->hash,
		    &snap_dummy)) {
			tessera_stat_aggregation_dedups++;
			struct tessera_pending_owner *po;
			while ((po = LIST_FIRST(&e->owners)) != NULL) {
				LIST_REMOVE(po, link);
				free(po, M_TESSERA);
			}
			free(e->bytes, M_TESSERA);
			free(e, M_TESSERA);
			continue;
		}

		/* Eligibility: only batch small manifests. Larger ones
		 * publish as single-blob packs (their pack overhead is
		 * already amortized over the body bytes). */
		if (e->len > (uint32_t)tessera_aggregation_blob_max) {
			if (err == 0) {
				int r = tessera_fs_publish_manifest_to_disk(
				    tmp_, e->bytes, e->len, e->hash);
				if (r != 0) err = r;
			}
			if (err == 0) {
				struct tessera_pending_owner *po;
				while ((po = LIST_FIRST(&e->owners)) != NULL) {
					LIST_REMOVE(po, link);
					free(po, M_TESSERA);
				}
				free(e->bytes, M_TESSERA);
				free(e, M_TESSERA);
			} else {
				mtx_lock(&tmp_->flush_mtx);
				uint32_t b = e->hash[0];
				LIST_INSERT_HEAD(&tmp_->pending_manifests[b],
				    e, link);
				tmp_->pending_manifest_count++;
				tmp_->pending_manifest_bytes += e->len;
				mtx_unlock(&tmp_->flush_mtx);
			}
			continue;
		}

		/* Add to current batch; flush if full. */
		if (bn == (uint32_t)tessera_aggregation_max_blobs ||
		    bbytes + e->len > (size_t)tessera_aggregation_max_bytes) {
			FLUSH_BATCH();
		}
		batch[bn].bytes = e->bytes;
		batch[bn].len   = e->len;
		memcpy(batch[bn].hash, e->hash, sizeof e->hash);
		batch_owners[bn] = e;
		bn++;
		bbytes += e->len;
	}
	FLUSH_BATCH();
#undef FLUSH_BATCH

	free(batch, M_TESSERA);
	free(batch_owners, M_TESSERA);
	return (err);
}

/* ── Per-inode dirty content buffer (write coalescing) ──────────
 *
 * INLINE-sized files (≤ 256 KiB) coalesce sequential small writes
 * into a per-inode RAM buffer. Without this, every 4 KiB write to
 * a 256 KiB file republishes the whole 256 KiB manifest — O(N²)
 * write amplification. With it, the manifest is published once at
 * fsync / flush / unmount.
 *
 * Lifecycle:
 *   vop_write (≤ INLINE)  → get_or_create + memcpy + mark dirty
 *   vop_write ( > INLINE) → drain_one then existing chunked path
 *   vop_read              → serve from buffer if present
 *   vop_setattr/truncate  → drain_one then existing path
 *   vop_fsync             → drain_one then existing flush
 *   tessera_fs_flush      → drain_all
 *   inode_delete          → drop (no publish)
 *
 * Locking: flush_mtx (same as dirty_inodes / pending_manifests).
 */

#define TESSERA_DIRTY_CONTENT_CAP_DEFAULT  (64u * 1024u * 1024u)
static unsigned long tessera_dirty_content_cap =
    TESSERA_DIRTY_CONTENT_CAP_DEFAULT;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, dirty_content_cap, CTLFLAG_RW,
    &tessera_dirty_content_cap, 0,
    "Per-mount cap on RAM held in vop_write coalescing buffers (bytes)");

/* Per-file ceiling — files larger than this take the chunked-write
 * path directly without coalescing (the buffer would have to hold
 * the whole file in RAM). 4 MiB covers typical small-write bursts
 * (build outputs, logs, scratch files) while keeping the per-inode
 * malloc bounded. */
static unsigned long tessera_dirty_content_file_max = 4u * 1024u * 1024u;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, dirty_content_file_max, CTLFLAG_RW,
    &tessera_dirty_content_file_max, 0,
    "Max per-file size eligible for the dirty-content buffer (bytes)");

static unsigned long tessera_stat_dirty_content_hits     = 0;
static unsigned long tessera_stat_dirty_content_creates  = 0;
static unsigned long tessera_stat_dirty_content_flushes  = 0;
static unsigned long tessera_stat_dirty_content_drops    = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, dirty_content_hits, CTLFLAG_RD,
    &tessera_stat_dirty_content_hits, 0,
    "vop_write/read served by the dirty-content buffer");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, dirty_content_creates, CTLFLAG_RD,
    &tessera_stat_dirty_content_creates, 0,
    "Dirty-content buffers allocated");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, dirty_content_flushes, CTLFLAG_RD,
    &tessera_stat_dirty_content_flushes, 0,
    "Dirty-content buffers published");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, dirty_content_drops, CTLFLAG_RD,
    &tessera_stat_dirty_content_drops, 0,
    "Dirty-content buffers dropped (unlink)");

/* ── CAS read cache (see docs/cas_cache_plan.md) ─────────────── */

/* Master enable. Setting to 0 disables both insertion and lookup —
 * the cache becomes a no-op. Lets us A/B without rebuilding the
 * kmod. */
static int tessera_cas_enable = 1;
SYSCTL_INT(_kern_tessera, OID_AUTO, cas_enable, CTLFLAG_RW,
    &tessera_cas_enable, 0,
    "Enable the CAS read cache (1=on, 0=off — disables both insert and lookup)");

/* Tier A — location entries. Default 16384 entries (~1 MiB). */
static unsigned long tessera_cas_loc_max = 16384;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_loc_max, CTLFLAG_RW,
    &tessera_cas_loc_max, 0,
    "Max number of CAS location entries (LRU-evicted past this)");

/* Tier B — bytes cache. Default 8 MiB (stage 5). */
static unsigned long tessera_cas_byte_max_bytes = 8u * 1024u * 1024u;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_byte_max_bytes, CTLFLAG_RW,
    &tessera_cas_byte_max_bytes, 0,
    "Max bytes held in the CAS bytes cache (LRU-evicted past this)");

/* Bytes cache eligibility — only blobs ≤ this size get cached as
 * bytes. Default 4 KiB matches the typical manifest / dirent-leaf
 * size. */
static unsigned long tessera_cas_small_blob_cap = 4096;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_small_blob_cap, CTLFLAG_RW,
    &tessera_cas_small_blob_cap, 0,
    "Max blob size eligible for the CAS bytes cache");

/* Stats — read-only, sum across all mounts via static globals.
 * Sufficient for single-mount workloads (the common case during
 * dev) and matches the convention used elsewhere in this file. */
static unsigned long tessera_stat_cas_loc_hits     = 0;
static unsigned long tessera_stat_cas_loc_misses   = 0;
static unsigned long tessera_stat_cas_loc_inserts  = 0;
static unsigned long tessera_stat_cas_loc_evicts   = 0;
static unsigned long tessera_stat_cas_byte_hits    = 0;
static unsigned long tessera_stat_cas_byte_misses  = 0;
static unsigned long tessera_stat_cas_byte_inserts = 0;
static unsigned long tessera_stat_cas_byte_evicts  = 0;
static unsigned long tessera_stat_cas_invalidations = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_loc_hits, CTLFLAG_RD,
    &tessera_stat_cas_loc_hits, 0, "CAS location-cache hits");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_loc_misses, CTLFLAG_RD,
    &tessera_stat_cas_loc_misses, 0, "CAS location-cache misses");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_loc_inserts, CTLFLAG_RD,
    &tessera_stat_cas_loc_inserts, 0, "CAS location-cache inserts");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_loc_evicts, CTLFLAG_RD,
    &tessera_stat_cas_loc_evicts, 0, "CAS location-cache LRU evictions");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_byte_hits, CTLFLAG_RD,
    &tessera_stat_cas_byte_hits, 0, "CAS bytes-cache hits");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_byte_misses, CTLFLAG_RD,
    &tessera_stat_cas_byte_misses, 0, "CAS bytes-cache misses");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_byte_inserts, CTLFLAG_RD,
    &tessera_stat_cas_byte_inserts, 0, "CAS bytes-cache inserts");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_byte_evicts, CTLFLAG_RD,
    &tessera_stat_cas_byte_evicts, 0, "CAS bytes-cache LRU evictions");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, cas_invalidations, CTLFLAG_RD,
    &tessera_stat_cas_invalidations, 0,
    "CAS-cache invalidations (pack delete / repack)");

/* Init / teardown. Init runs from mountfs once flush-time
 * structures exist; teardown runs from unmount before mount struct
 * is freed. Both are no-ops if mtx_init was never set. */
static void
tessera_cas_cache_init(struct tessera_cas_cache *c)
{
	memset(c, 0, sizeof *c);
	mtx_init(&c->mtx, "tess_cas", NULL, MTX_DEF);
	c->mtx_init = 1;
	for (uint32_t b = 0; b < TESSERA_CAS_LOC_BUCKETS; b++)
		LIST_INIT(&c->loc_buckets[b]);
	TAILQ_INIT(&c->loc_lru);
	for (uint32_t b = 0; b < TESSERA_CAS_BYTE_BUCKETS; b++)
		LIST_INIT(&c->byte_buckets[b]);
	TAILQ_INIT(&c->byte_lru);
}

static void
tessera_cas_cache_drain(struct tessera_cas_cache *c)
{
	if (!c->mtx_init) return;
	mtx_lock(&c->mtx);
	struct tessera_cas_loc_entry *le;
	while ((le = TAILQ_FIRST(&c->loc_lru)) != NULL) {
		TAILQ_REMOVE(&c->loc_lru, le, lru_link);
		LIST_REMOVE(le, hash_link);
		free(le, M_TESSERA);
	}
	c->loc_count = 0;
	struct tessera_cas_byte_entry *be;
	while ((be = TAILQ_FIRST(&c->byte_lru)) != NULL) {
		TAILQ_REMOVE(&c->byte_lru, be, lru_link);
		LIST_REMOVE(be, hash_link);
		if (be->bytes != NULL) free(be->bytes, M_TESSERA);
		free(be, M_TESSERA);
	}
	c->byte_bytes = 0;
	mtx_unlock(&c->mtx);
	mtx_destroy(&c->mtx);
	c->mtx_init = 0;
}

/* Bucket selector — the hash is already cryptographic, so the low
 * 32 bits are uniform. */
static inline uint32_t
tessera_cas_loc_bucket(const tessera_hash_t h)
{
	uint32_t v;
	memcpy(&v, h, sizeof v);
	return (v & (TESSERA_CAS_LOC_BUCKETS - 1u));
}

/* Insert a location entry. No-op if the cache is disabled or low
 * memory. Caller does NOT hold the cache mtx.
 *
 * Stage 2: called from publish_*_to_disk after a successful
 * btree_put. We know exactly where the pack landed and how many
 * sectors it occupies — the cheapest possible source of truth. */
static void
tessera_cas_loc_insert(struct tessera_cas_cache *c,
                       const tessera_hash_t hash,
                       const uint8_t pack_id[16],
                       const tessera_pack_extent_t *exts, uint32_t nexts,
                       uint64_t total_sectors)
{
	if (!tessera_cas_enable || !c->mtx_init) return;

	struct tessera_cas_loc_entry *e =
	    malloc(sizeof *e, M_TESSERA, M_NOWAIT | M_ZERO);
	if (e == NULL) return;
	memcpy(e->hash, hash, sizeof e->hash);
	memcpy(e->pack_id, pack_id, sizeof e->pack_id);
	e->total_sectors = total_sectors;
	if (nexts <= 4) {
		e->n_extents = (uint8_t)nexts;
		for (uint32_t i = 0; i < nexts; i++) e->extents[i] = exts[i];
	} else {
		e->n_extents = 0xFF; /* signal: use resolver on read */
	}

	uint32_t b = tessera_cas_loc_bucket(hash);
	mtx_lock(&c->mtx);
	/* Replace any existing entry for this hash (re-publish updates
	 * the location). */
	struct tessera_cas_loc_entry *existing;
	LIST_FOREACH(existing, &c->loc_buckets[b], hash_link) {
		if (memcmp(existing->hash, hash, sizeof existing->hash) == 0) {
			TAILQ_REMOVE(&c->loc_lru, existing, lru_link);
			LIST_REMOVE(existing, hash_link);
			c->loc_count--;
			free(existing, M_TESSERA);
			break;
		}
	}
	LIST_INSERT_HEAD(&c->loc_buckets[b], e, hash_link);
	TAILQ_INSERT_HEAD(&c->loc_lru, e, lru_link);
	c->loc_count++;
	tessera_stat_cas_loc_inserts++;
	/* Strict LRU eviction past the cap. */
	while (c->loc_count > (size_t)tessera_cas_loc_max) {
		struct tessera_cas_loc_entry *victim =
		    TAILQ_LAST(&c->loc_lru, tessera_cas_loc_lru);
		if (victim == NULL) break;
		TAILQ_REMOVE(&c->loc_lru, victim, lru_link);
		LIST_REMOVE(victim, hash_link);
		c->loc_count--;
		tessera_stat_cas_loc_evicts++;
		free(victim, M_TESSERA);
	}
	mtx_unlock(&c->mtx);
}

/* Drop every cache entry pointing at the given pack_id. Called when
 * a pack is deleted (gc_data_zone) or relocated (repack). O(loc_count)
 * but invalidations are rare relative to lookups.
 *
 * Also drops bytes-cache entries with matching hashes — we walk the
 * loc entries we're dropping and look each one up in bytes (cheap:
 * O(1) per loc entry). */
static void
tessera_cas_invalidate_pack(struct tessera_cas_cache *c,
                            const uint8_t pack_id[16])
{
	if (!c->mtx_init) return;
	mtx_lock(&c->mtx);
	struct tessera_cas_loc_entry *e, *next;
	TAILQ_FOREACH_SAFE(e, &c->loc_lru, lru_link, next) {
		if (memcmp(e->pack_id, pack_id, 16) == 0) {
			/* Also drop the bytes-cache entry for this hash, if
			 * any — same hash → same bytes, but the on-disk
			 * source is gone or moved. (Bytes are still
			 * correct, but invalidation gives a uniform
			 * "post-invalidate, both tiers are clean" rule.) */
			uint32_t bb;
			memcpy(&bb, e->hash, sizeof bb);
			bb &= (TESSERA_CAS_BYTE_BUCKETS - 1u);
			struct tessera_cas_byte_entry *be, *bnext;
			LIST_FOREACH_SAFE(be, &c->byte_buckets[bb],
			                   hash_link, bnext) {
				if (memcmp(be->hash, e->hash,
				    sizeof be->hash) == 0) {
					TAILQ_REMOVE(&c->byte_lru, be,
					    lru_link);
					LIST_REMOVE(be, hash_link);
					if (c->byte_bytes >= be->length)
						c->byte_bytes -= be->length;
					else
						c->byte_bytes = 0;
					if (be->bytes != NULL)
						free(be->bytes, M_TESSERA);
					free(be, M_TESSERA);
					tessera_stat_cas_byte_evicts++;
					break;
				}
			}
			TAILQ_REMOVE(&c->loc_lru, e, lru_link);
			LIST_REMOVE(e, hash_link);
			c->loc_count--;
			tessera_stat_cas_invalidations++;
			free(e, M_TESSERA);
		}
	}
	mtx_unlock(&c->mtx);
}

static inline uint32_t
tessera_cas_byte_bucket(const tessera_hash_t h)
{
	uint32_t v;
	memcpy(&v, h, sizeof v);
	return (v & (TESSERA_CAS_BYTE_BUCKETS - 1u));
}

static int
tessera_cas_byte_lookup(struct tessera_cas_cache *c,
                        const tessera_hash_t hash,
                        uint8_t **out_buf, uint32_t *out_len)
{
	if (!tessera_cas_enable || !c->mtx_init ||
	    tessera_cas_byte_max_bytes == 0) {
		tessera_stat_cas_byte_misses++;
		return (0);
	}
	uint32_t b = tessera_cas_byte_bucket(hash);
	mtx_lock(&c->mtx);
	struct tessera_cas_byte_entry *e;
	LIST_FOREACH(e, &c->byte_buckets[b], hash_link) {
		if (memcmp(e->hash, hash, sizeof e->hash) == 0) {
			/* Copy bytes under the lock — uiomove-style copy
			 * out happens at the caller. */
			uint8_t *copy = malloc(e->length, M_TESSERA, M_NOWAIT);
			if (copy == NULL) {
				/* Treat as miss; caller will fetch + reinsert. */
				mtx_unlock(&c->mtx);
				tessera_stat_cas_byte_misses++;
				return (0);
			}
			memcpy(copy, e->bytes, e->length);
			*out_buf = copy;
			*out_len = e->length;
			TAILQ_REMOVE(&c->byte_lru, e, lru_link);
			TAILQ_INSERT_HEAD(&c->byte_lru, e, lru_link);
			tessera_stat_cas_byte_hits++;
			mtx_unlock(&c->mtx);
			return (1);
		}
	}
	tessera_stat_cas_byte_misses++;
	mtx_unlock(&c->mtx);
	return (0);
}

static void
tessera_cas_byte_insert(struct tessera_cas_cache *c,
                        const tessera_hash_t hash,
                        const uint8_t *bytes, uint32_t length)
{
	if (!tessera_cas_enable || !c->mtx_init ||
	    tessera_cas_byte_max_bytes == 0)
		return;
	if (length == 0 || length > (uint32_t)tessera_cas_small_blob_cap)
		return;

	uint8_t *copy = malloc(length, M_TESSERA, M_NOWAIT);
	if (copy == NULL) return;
	memcpy(copy, bytes, length);

	struct tessera_cas_byte_entry *e =
	    malloc(sizeof *e, M_TESSERA, M_NOWAIT | M_ZERO);
	if (e == NULL) { free(copy, M_TESSERA); return; }
	memcpy(e->hash, hash, sizeof e->hash);
	e->length = length;
	e->bytes  = copy;

	uint32_t b = tessera_cas_byte_bucket(hash);
	mtx_lock(&c->mtx);
	/* Replace existing entry for this hash. */
	struct tessera_cas_byte_entry *existing;
	LIST_FOREACH(existing, &c->byte_buckets[b], hash_link) {
		if (memcmp(existing->hash, hash, sizeof existing->hash) == 0) {
			TAILQ_REMOVE(&c->byte_lru, existing, lru_link);
			LIST_REMOVE(existing, hash_link);
			if (c->byte_bytes >= existing->length)
				c->byte_bytes -= existing->length;
			else
				c->byte_bytes = 0;
			if (existing->bytes != NULL)
				free(existing->bytes, M_TESSERA);
			free(existing, M_TESSERA);
			break;
		}
	}
	LIST_INSERT_HEAD(&c->byte_buckets[b], e, hash_link);
	TAILQ_INSERT_HEAD(&c->byte_lru, e, lru_link);
	c->byte_bytes += length;
	tessera_stat_cas_byte_inserts++;
	/* Evict LRU until under the byte cap. */
	while (c->byte_bytes > (size_t)tessera_cas_byte_max_bytes) {
		struct tessera_cas_byte_entry *victim =
		    TAILQ_LAST(&c->byte_lru, tessera_cas_byte_lru);
		if (victim == NULL || victim == e) break;
		TAILQ_REMOVE(&c->byte_lru, victim, lru_link);
		LIST_REMOVE(victim, hash_link);
		if (c->byte_bytes >= victim->length)
			c->byte_bytes -= victim->length;
		else
			c->byte_bytes = 0;
		if (victim->bytes != NULL) free(victim->bytes, M_TESSERA);
		free(victim, M_TESSERA);
		tessera_stat_cas_byte_evicts++;
	}
	mtx_unlock(&c->mtx);
}

/* Look up a hash. Returns 1 on hit (snapshot filled in), 0 on miss.
 * Updates LRU position on hit. Caller does NOT hold cache mtx. */
static int
tessera_cas_loc_lookup(struct tessera_cas_cache *c,
                       const tessera_hash_t hash,
                       struct tessera_cas_loc_snap *out)
{
	if (!tessera_cas_enable || !c->mtx_init) {
		tessera_stat_cas_loc_misses++;
		return (0);
	}
	uint32_t b = tessera_cas_loc_bucket(hash);
	mtx_lock(&c->mtx);
	struct tessera_cas_loc_entry *e;
	LIST_FOREACH(e, &c->loc_buckets[b], hash_link) {
		if (memcmp(e->hash, hash, sizeof e->hash) == 0) {
			memcpy(out->pack_id, e->pack_id, sizeof out->pack_id);
			out->total_sectors = e->total_sectors;
			out->n_extents     = e->n_extents;
			if (e->n_extents <= 4) {
				for (uint32_t i = 0; i < e->n_extents; i++)
					out->extents[i] = e->extents[i];
			}
			TAILQ_REMOVE(&c->loc_lru, e, lru_link);
			TAILQ_INSERT_HEAD(&c->loc_lru, e, lru_link);
			tessera_stat_cas_loc_hits++;
			mtx_unlock(&c->mtx);
			return (1);
		}
	}
	tessera_stat_cas_loc_misses++;
	mtx_unlock(&c->mtx);
	return (0);
}

/* Caller holds flush_mtx. */
static struct tessera_dirty_content *
tessera_fs_dirty_content_lookup(struct tessera_mount *tmp_, uint32_t inode_no)
{
	uint32_t b = inode_no & (TESSERA_DIRTY_CONTENT_BUCKETS - 1u);
	struct tessera_dirty_content *dc;
	LIST_FOREACH(dc, &tmp_->dirty_content[b], link)
		if (dc->inode_no == inode_no) return (dc);
	return (NULL);
}

/* Caller holds flush_mtx. Detaches from list, updates accounting. */
static void
tessera_fs_dirty_content_detach(struct tessera_mount *tmp_,
                                struct tessera_dirty_content *dc)
{
	LIST_REMOVE(dc, link);
	if (tmp_->dirty_content_bytes >= dc->size)
		tmp_->dirty_content_bytes -= dc->size;
	else
		tmp_->dirty_content_bytes = 0;
}

static void
tessera_fs_dirty_content_free(struct tessera_dirty_content *dc)
{
	if (dc == NULL) return;
	if (dc->bytes != NULL) free(dc->bytes, M_TESSERA);
	free(dc, M_TESSERA);
}

/* Publish a detached buffer to disk. Routes to the INLINE or chunked
 * publish path based on size — the caller's vop_write doesn't have
 * to choose, the buffer just remembers the contents. */
static int
tessera_fs_dirty_content_publish(struct tessera_mount *tmp_,
                                 struct tessera_dirty_content *dc)
{
	if (!dc->dirty) return (0);
	int rc;
	/* TESSERA_INLINE_THRESHOLD = 256 KiB; defined later in file. */
	if (dc->size <= (256u * 1024u)) {
		rc = tessera_fs_replace_content(tmp_, dc->inode_no,
		    dc->bytes, dc->size);
		if (rc == 0) tessera_stat_vop_write_inline++;
	} else {
		rc = tessera_fs_replace_content_chunked(tmp_, dc->inode_no,
		    dc->bytes, dc->size);
		if (rc == 0) tessera_stat_vop_write_chunked++;
	}
	if (rc == 0) tessera_stat_dirty_content_flushes++;
	return (rc);
}

/* Drain a single inode's buffer (vop_fsync / pre-truncate /
 * pre-chunked-write). Detaches under lock, publishes unlocked. */
static int
tessera_fs_dirty_content_drain_one(struct tessera_mount *tmp_,
                                   uint32_t inode_no)
{
	if (!tmp_->flush_mtx_init) return (0);
	mtx_lock(&tmp_->flush_mtx);
	struct tessera_dirty_content *dc =
	    tessera_fs_dirty_content_lookup(tmp_, inode_no);
	if (dc == NULL) {
		mtx_unlock(&tmp_->flush_mtx);
		return (0);
	}
	tessera_fs_dirty_content_detach(tmp_, dc);
	mtx_unlock(&tmp_->flush_mtx);
	int rc = tessera_fs_dirty_content_publish(tmp_, dc);
	tessera_fs_dirty_content_free(dc);
	return (rc);
}

/* Drain every buffer (called from tessera_fs_flush). */
static int
tessera_fs_dirty_content_drain_all(struct tessera_mount *tmp_)
{
	if (!tmp_->flush_mtx_init) return (0);
	int last_err = 0;
	for (uint32_t b = 0; b < TESSERA_DIRTY_CONTENT_BUCKETS; b++) {
		for (;;) {
			mtx_lock(&tmp_->flush_mtx);
			struct tessera_dirty_content *dc =
			    LIST_FIRST(&tmp_->dirty_content[b]);
			if (dc == NULL) {
				mtx_unlock(&tmp_->flush_mtx);
				break;
			}
			tessera_fs_dirty_content_detach(tmp_, dc);
			mtx_unlock(&tmp_->flush_mtx);
			int rc = tessera_fs_dirty_content_publish(tmp_, dc);
			if (rc != 0) last_err = rc;
			tessera_fs_dirty_content_free(dc);
		}
	}
	return (last_err);
}

/* Drop without publishing (called on unlink). */
static void
tessera_fs_dirty_content_drop(struct tessera_mount *tmp_, uint32_t inode_no)
{
	if (!tmp_->flush_mtx_init) return;
	mtx_lock(&tmp_->flush_mtx);
	struct tessera_dirty_content *dc =
	    tessera_fs_dirty_content_lookup(tmp_, inode_no);
	if (dc != NULL) {
		tessera_fs_dirty_content_detach(tmp_, dc);
		tessera_stat_dirty_content_drops++;
	}
	mtx_unlock(&tmp_->flush_mtx);
	tessera_fs_dirty_content_free(dc);
}

/* uio variant — copy directly from userspace into the per-inode
 * buffer. Avoids the kbuf malloc + extra memcpy on the vop_write
 * fast path. The vnode lock (held EXCLUSIVE by VFS during vop_write)
 * guarantees no concurrent drain_one on this inode, so the bytes
 * pointer we sample under flush_mtx stays valid across the unlocked
 * uiomove.
 *
 * Returns 0 on success; on uiomove failure, returns the error and
 * the buffer's size/dirty are NOT advanced (the partially-written
 * region is overwritten by the next successful write or zeroed at
 * grow time). */
static int
tessera_fs_dirty_content_write_uio(struct tessera_mount *tmp_,
                                   uint32_t inode_no, uint64_t write_off,
                                   struct uio *uio, size_t final_size)
{
	const size_t write_len = (size_t)uio->uio_resid;

	mtx_lock(&tmp_->flush_mtx);
	int over_cap = (tmp_->dirty_content_bytes + final_size >
	    (size_t)tessera_dirty_content_cap);
	mtx_unlock(&tmp_->flush_mtx);
	if (over_cap)
		(void)tessera_fs_dirty_content_drain_all(tmp_);

	/* Get or create the buffer; grow if needed. Same logic as the
	 * non-uio variant, but stops short of the memcpy and instead
	 * hands back a pointer for the caller-side uiomove. */
	uint8_t *target_bytes = NULL;
	mtx_lock(&tmp_->flush_mtx);
	struct tessera_dirty_content *dc =
	    tessera_fs_dirty_content_lookup(tmp_, inode_no);
	if (dc == NULL) {
		mtx_unlock(&tmp_->flush_mtx);
		/* Slow path — fall back to the kbuf variant which
		 * handles read_full_content. (First-touch is uncommon
		 * relative to subsequent writes.) */
		uint8_t *kbuf = malloc(write_len, M_TESSERA, M_WAITOK);
		int err = uiomove(kbuf, (int)write_len, uio);
		if (err != 0) { free(kbuf, M_TESSERA); return (err); }
		err = tessera_fs_dirty_content_write(tmp_, inode_no,
		    write_off, kbuf, write_len, final_size);
		free(kbuf, M_TESSERA);
		return (err);
	}

	if (final_size > dc->capacity) {
		size_t new_cap = dc->capacity ? dc->capacity * 2 : 4096;
		while (new_cap < final_size) new_cap *= 2;
		if (new_cap > (size_t)tessera_dirty_content_file_max)
			new_cap = (size_t)tessera_dirty_content_file_max;
		if (new_cap < final_size) new_cap = final_size;
		uint8_t *nb = malloc(new_cap, M_TESSERA, M_NOWAIT | M_ZERO);
		if (nb == NULL) {
			mtx_unlock(&tmp_->flush_mtx);
			return (ENOMEM);
		}
		if (dc->size > 0)
			memcpy(nb, dc->bytes, dc->size);
		free(dc->bytes, M_TESSERA);
		dc->bytes    = nb;
		dc->capacity = new_cap;
	}
	if (write_off > dc->size)
		memset(dc->bytes + dc->size, 0,
		    (size_t)(write_off - dc->size));
	target_bytes = dc->bytes + write_off;
	mtx_unlock(&tmp_->flush_mtx);

	/* Direct copy from user — vnode is held exclusive so dc->bytes
	 * cannot be freed under us. */
	int err = uiomove(target_bytes, (int)write_len, uio);
	if (err != 0) return (err);

	mtx_lock(&tmp_->flush_mtx);
	/* Re-lookup defensively; in normal operation dc is still the
	 * same entry, but a concurrent invalidate-by-inode could have
	 * dropped it (we don't have that path today, but keep the
	 * structure honest). */
	dc = tessera_fs_dirty_content_lookup(tmp_, inode_no);
	if (dc == NULL) {
		mtx_unlock(&tmp_->flush_mtx);
		return (0);
	}
	size_t new_size = (size_t)(write_off + write_len) > dc->size
	    ? (size_t)(write_off + write_len) : dc->size;
	if (new_size > final_size) new_size = final_size;
	if (new_size > dc->size)
		tmp_->dirty_content_bytes += (new_size - dc->size);
	dc->size  = new_size;
	dc->dirty = 1;
	tessera_stat_dirty_content_hits++;
	mtx_unlock(&tmp_->flush_mtx);
	return (0);
}

/* Apply a write into the per-inode buffer. Buffer is created on
 * first touch by reading the existing content. final_size is the
 * post-write file size; must be ≤ tessera_dirty_content_file_max
 * (4 MiB by default). Larger files take the chunked path directly.
 *
 * Caller does NOT hold flush_mtx. Returns 0 on success.
 */
static int
tessera_fs_dirty_content_write(struct tessera_mount *tmp_, uint32_t inode_no,
                               uint64_t write_off, const uint8_t *new_bytes,
                               size_t write_len, size_t final_size)
{

	/* Memory cap: if this write would push us past the cap, drain
	 * everything first. Coarse but correct; refine later. */
	mtx_lock(&tmp_->flush_mtx);
	int over_cap = (tmp_->dirty_content_bytes + final_size >
	    (size_t)tessera_dirty_content_cap);
	mtx_unlock(&tmp_->flush_mtx);
	if (over_cap) {
		(void)tessera_fs_dirty_content_drain_all(tmp_);
	}

	/* Look up or create the buffer. We may need to materialise the
	 * existing on-disk content for the first dirty — do that without
	 * holding flush_mtx (read_full_content can sleep / fetch_blob). */
	mtx_lock(&tmp_->flush_mtx);
	struct tessera_dirty_content *dc =
	    tessera_fs_dirty_content_lookup(tmp_, inode_no);
	if (dc != NULL) {
		/* Grow buffer geometrically when needed. Without doubling,
		 * a sequential dd of N writes triggers N reallocs and
		 * O(N²) memcpy total — for 256 4-KiB writes that's ~131
		 * MiB of memcpy on a 1 MiB file. With doubling it's
		 * O(log N) reallocs and O(N) total memcpy. */
		if (final_size > dc->capacity) {
			size_t new_cap = dc->capacity ? dc->capacity * 2 : 4096;
			while (new_cap < final_size) new_cap *= 2;
			if (new_cap > (size_t)tessera_dirty_content_file_max)
				new_cap = (size_t)tessera_dirty_content_file_max;
			if (new_cap < final_size) new_cap = final_size;
			uint8_t *nb = malloc(new_cap, M_TESSERA,
			    M_NOWAIT | M_ZERO);
			if (nb == NULL) {
				mtx_unlock(&tmp_->flush_mtx);
				return (ENOMEM);
			}
			if (dc->size > 0)
				memcpy(nb, dc->bytes, dc->size);
			free(dc->bytes, M_TESSERA);
			dc->bytes    = nb;
			dc->capacity = new_cap;
		}
		/* Zero-fill any gap from old size up to write_off. */
		if (write_off > dc->size)
			memset(dc->bytes + dc->size, 0,
			    (size_t)(write_off - dc->size));
		memcpy(dc->bytes + write_off, new_bytes, write_len);
		size_t new_size = (size_t)(write_off + write_len) > dc->size
		    ? (size_t)(write_off + write_len) : dc->size;
		if (new_size > final_size) new_size = final_size;
		if (new_size > dc->size)
			tmp_->dirty_content_bytes += (new_size - dc->size);
		dc->size  = new_size;
		dc->dirty = 1;
		tessera_stat_dirty_content_hits++;
		mtx_unlock(&tmp_->flush_mtx);
		return (0);
	}
	mtx_unlock(&tmp_->flush_mtx);

	/* First touch — fetch existing on-disk content. */
	tessera_inode_record_t ino;
	if (tessera_fs_inode_get(tmp_, inode_no, &ino) != TESSERA_OK)
		return (EIO);
	uint8_t *old_buf = NULL;
	size_t   old_len = 0;
	if (ino.size > 0) {
		if (tessera_fs_read_full_content(tmp_, &ino, &old_buf,
		    &old_len) != 0)
			return (EIO);
	}

	struct tessera_dirty_content *ndc =
	    malloc(sizeof *ndc, M_TESSERA, M_WAITOK | M_ZERO);
	ndc->inode_no = inode_no;
	/* Start with at least 64 KiB so a typical sequential dd doesn't
	 * pay log2(file_size / 4 KiB) ≈ 6 reallocs to climb out of the
	 * tiny range. Cap at file_max so we never over-allocate beyond
	 * the threshold that routes through this buffer at all. */
	{
		size_t cap = final_size;
		if (cap < 64u * 1024u) cap = 64u * 1024u;
		if (cap > (size_t)tessera_dirty_content_file_max)
			cap = (size_t)tessera_dirty_content_file_max;
		if (cap < final_size) cap = final_size;
		ndc->capacity = cap;
	}
	ndc->bytes    = malloc(ndc->capacity, M_TESSERA, M_WAITOK | M_ZERO);
	if (old_buf != NULL) {
		size_t n = old_len < final_size ? old_len : final_size;
		memcpy(ndc->bytes, old_buf, n);
		free(old_buf, M_TESSERA);
	}
	memcpy(ndc->bytes + write_off, new_bytes, write_len);
	size_t new_size = (size_t)(write_off + write_len) > (size_t)ino.size
	    ? (size_t)(write_off + write_len) : (size_t)ino.size;
	if (new_size > final_size) new_size = final_size;
	ndc->size  = new_size;
	ndc->dirty = 1;

	mtx_lock(&tmp_->flush_mtx);
	/* Race: another thread may have created an entry while we were
	 * fetching. If so, drop ours and recurse — the cheap path will
	 * win. */
	struct tessera_dirty_content *existing =
	    tessera_fs_dirty_content_lookup(tmp_, inode_no);
	if (existing != NULL) {
		mtx_unlock(&tmp_->flush_mtx);
		tessera_fs_dirty_content_free(ndc);
		return (tessera_fs_dirty_content_write(tmp_, inode_no,
		    write_off, new_bytes, write_len, final_size));
	}
	uint32_t b = inode_no & (TESSERA_DIRTY_CONTENT_BUCKETS - 1u);
	LIST_INSERT_HEAD(&tmp_->dirty_content[b], ndc, link);
	tmp_->dirty_content_bytes += ndc->size;
	tessera_stat_dirty_content_creates++;
	mtx_unlock(&tmp_->flush_mtx);
	return (0);
}

/* Read from the buffer if present. Returns 1 if served (uio
 * advanced), 0 if no buffer (caller falls through to disk),
 * negative errno on error. */
static int
tessera_fs_dirty_content_read(struct tessera_mount *tmp_, uint32_t inode_no,
                              struct uio *uio)
{
	if (!tmp_->flush_mtx_init) return (0);
	mtx_lock(&tmp_->flush_mtx);
	struct tessera_dirty_content *dc =
	    tessera_fs_dirty_content_lookup(tmp_, inode_no);
	if (dc == NULL) {
		mtx_unlock(&tmp_->flush_mtx);
		return (0);
	}
	/* Snapshot bytes under the lock — uiomove can sleep. */
	if ((uint64_t)uio->uio_offset >= dc->size) {
		mtx_unlock(&tmp_->flush_mtx);
		tessera_stat_dirty_content_hits++;
		return (1);
	}
	size_t avail = dc->size - (size_t)uio->uio_offset;
	size_t n = (uio->uio_resid < (ssize_t)avail)
	    ? (size_t)uio->uio_resid : avail;
	uint8_t *snap = malloc(n, M_TESSERA, M_NOWAIT);
	if (snap == NULL) {
		mtx_unlock(&tmp_->flush_mtx);
		return (-ENOMEM);
	}
	memcpy(snap, dc->bytes + uio->uio_offset, n);
	mtx_unlock(&tmp_->flush_mtx);
	int err = uiomove(snap, (int)n, uio);
	free(snap, M_TESSERA);
	tessera_stat_dirty_content_hits++;
	return (err == 0 ? 1 : -err);
}

static int
tessera_fs_flush(struct tessera_mount *tmp_)
{
	/* Pre-mtx-init paths (mount-time GC during mountfs) get the
	 * unsynchronised behaviour. */
	if (!tmp_->flush_mtx_init) {
		if (!tmp_->sb_dirty) return (0);
		int r = tessera_commit_sb(tmp_);
		if (r == 0) tmp_->sb_dirty = 0;
		return (r);
	}

	/* Publish any coalesced INLINE writes first; they feed dirty
	 * inodes / pending manifests that the rest of this function
	 * then drains. Done outside flush_in_progress so concurrent
	 * fsync waiters still benefit from group commit. */
	(void)tessera_fs_dirty_content_drain_all(tmp_);

	mtx_lock(&tmp_->flush_mtx);
	for (;;) {
		if (!tmp_->sb_dirty) {
			mtx_unlock(&tmp_->flush_mtx);
			return (0);
		}
		if (tmp_->flush_in_progress) {
			/* Another thread is already committing; wait for
			 * it. When we wake we re-check sb_dirty: if it's
			 * cleared the active commit covered our changes
			 * (any vop that set sb_dirty completed before we
			 * called fsync), so we're done. If sb_dirty is
			 * still 1 our writes happened post-flush — loop
			 * to start our own commit. */
			tessera_stat_fsync_group_wait++;
			(void)msleep(&tmp_->flush_in_progress,
			    &tmp_->flush_mtx, PRIBIO, "tessflsh", 0);
			continue;
		}
		tmp_->flush_in_progress = 1;
		break;
	}
	mtx_unlock(&tmp_->flush_mtx);

	/* v2.6 B.2: force a synchronous group commit of the
	 * journal-pending list before checkpoint. Two reasons: (a) any
	 * still-pending records become durable as part of THIS flush,
	 * tightening the post-fsync durability window; (b) records
	 * that made it to the journal but whose checkpoint applied
	 * them to BTREE are about to be subsumed by commit_sb's
	 * journal_checkpoint, so writing them again now is harmless.
	 *
	 * After this call, the journal-pending list is empty. After
	 * commit_sb's journal_checkpoint at the end of this flush,
	 * the just-written DIR_INSERT/REMOVE records are also gone
	 * from the journal — they're already reflected in the new
	 * SB roots. */
	(void)tessera_fs_journal_log_drain(tmp_);

	/* v2.6: checkpoint the dirent log BEFORE the manifest /
	 * inode-tree drains. Each dirty parent gets a single bulk
	 * BTREE rebuild that incorporates the entire batch of pending
	 * dirent ops; the rebuild itself produces NEW manifest publishes
	 * + inode_tree updates which feed into the existing drains
	 * naturally. */
	(void)tessera_fs_dirent_log_checkpoint_all(tmp_);

	/* Drain dirty inodes BEFORE commit_sb so the SB sectors written
	 * by commit_sb capture the post-drain inode_root. drain itself
	 * takes flush_mtx briefly per entry. */
	/* Drain pending manifests FIRST — inode_tree records reference
	 * their hashes, so the packs must hit disk before drain →
	 * btree_put runs. Then drain dirty inodes. Each drain consumes
	 * extent allocations (publish_manifest_to_disk → extent_alloc;
	 * btree_put → meta-reserve). commit_extent flushes the
	 * in-memory free-extent allocator's deltas to a fresh on-disk
	 * tree BEFORE commit_sb writes the SB sectors — without it,
	 * sb.free_extent_root would point at a stale view that misses
	 * sectors allocated during this drain. */
	/* Pre-pass: drain inode tombstones and run GC so the just-deleted
	 * files' content packs become reclaimable BEFORE pending_manifests_
	 * drain tries to allocate fresh sectors. Critical for rm-heavy
	 * workloads where the data zone would otherwise fill to 100%
	 * (each rm publishes a new dir manifest pack while the unlinked
	 * files' content packs sit in registry until next mount-time GC).
	 *
	 * Skip in the snapshot-readonly path (no inode mutations to drain)
	 * and when there are no tombstones (gc_data_zone is O(N inodes ×
	 * N packs), don't pay the cost on the steady-state read-mostly
	 * flush). */
	int has_tombstones = 0;
	mtx_lock(&tmp_->flush_mtx);
	for (uint32_t b = 0; b < TESSERA_DIRTY_INODE_BUCKETS &&
	    !has_tombstones; b++) {
		struct tessera_dirty_inode *e;
		LIST_FOREACH(e, &tmp_->dirty_inodes[b], link) {
			if (e->tombstone) { has_tombstones = 1; break; }
		}
	}
	mtx_unlock(&tmp_->flush_mtx);
	if (has_tombstones && !tmp_->readonly_snapshot) {
		(void)tessera_fs_dirty_inodes_drain_tombstones(tmp_);
		(void)tessera_fs_gc_data_zone(tmp_);
	}

	int r = tessera_fs_pending_manifests_drain(tmp_);
	if (r != 0)
		printf("tessera_fs: flush — pending_manifests_drain failed: %d "
		    "(unmounting=%d sb_dirty=%d)\n",
		    r, tmp_->flush_unmounting, tmp_->sb_dirty);
	if (r == 0) {
		r = tessera_fs_dirty_inodes_drain(tmp_);
		if (r != 0)
			printf("tessera_fs: flush — dirty_inodes_drain failed: "
			    "%d (unmounting=%d sb_dirty=%d)\n",
			    r, tmp_->flush_unmounting, tmp_->sb_dirty);
	}
	if (r == 0) {
		int rce = tessera_commit_extent(tmp_);
		if (rce != 0)
			printf("tessera_fs: flush — commit_extent failed: %d "
			    "(unmounting=%d sb_dirty=%d)\n",
			    rce, tmp_->flush_unmounting, tmp_->sb_dirty);
	}
	if (r == 0) {
		r = tessera_commit_sb(tmp_);
		if (r != 0)
			printf("tessera_fs: flush — commit_sb failed: %d "
			    "(unmounting=%d)\n", r, tmp_->flush_unmounting);
	}

	mtx_lock(&tmp_->flush_mtx);
	if (r == 0) tmp_->sb_dirty = 0;
	tmp_->flush_in_progress = 0;
	wakeup(&tmp_->flush_in_progress);
	mtx_unlock(&tmp_->flush_mtx);
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
 * Rebuild meta_pin_bitmap from scratch by walking every meta-reserve
 * sector that's reachable from a root the SB still cares about: the
 * four live trees + every retained snapshot's three trees. Used at
 * mount time and at retention time (commit_sb drops oldest snapshot).
 *
 * Without a mid-session rebuild, sectors pinned by a now-retired
 * snapshot stay pinned in meta_pending until the next mount — meaning
 * sustained-write workloads exhaust the meta-reserve in a few hundred
 * commits even with a small retention horizon (bug #2).
 *
 * Caller must hold flush_in_progress (so live trees aren't being COW'd
 * underfoot). Mount-time caller is single-threaded; commit_sb caller
 * is serialised by flush_in_progress.
 */
static void
tessera_meta_pin_bitmap_rebuild(struct tessera_mount *tmp_)
{
	if (tmp_->meta_pin_bitmap == NULL) return;
	memset(tmp_->meta_pin_bitmap, 0, tmp_->meta_pin_bitmap_bytes);

	struct meta_mark_ctx mctx = {
		tmp_->meta_pin_bitmap,
		tmp_->sb.meta_reserve_start,
		tmp_->sb.meta_reserve_start + tmp_->sb.meta_reserve_length,
	};

	if (tmp_->inode_tree != NULL)
		(void)tessera_btree_walk_nodes(tmp_->inode_tree,
		    meta_mark_visitor, &mctx);
	if (tmp_->pack_registry_tree != NULL)
		(void)tessera_btree_walk_nodes(tmp_->pack_registry_tree,
		    meta_mark_visitor, &mctx);
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
	if (tmp_->snapshots_tree == NULL) return;

	(void)tessera_btree_walk_nodes(tmp_->snapshots_tree,
	    meta_mark_visitor, &mctx);
	tessera_btree_cursor_t *sc =
	    tessera_btree_seek_first(tmp_->snapshots_tree);
	while (sc != NULL) {
		uint8_t sk[8];
		tessera_snapshot_record_t srec;
		if (tessera_btree_cursor_get(sc, sk, &srec) != TESSERA_OK)
			break;
		if (srec.inode_root != 0 &&
		    srec.inode_root != tmp_->sb.inode_root) {
			tessera_btree_t *t = tessera_btree_open(&tmp_->meta_bio,
			    srec.inode_root, /*tree_kind*/ 0, /*key*/ 4,
			    /*value*/ TESSERA_INODE_RECORD_SIZE);
			if (t != NULL) {
				(void)tessera_btree_walk_nodes(t,
				    meta_mark_visitor, &mctx);
				tessera_btree_close(t);
			}
		}
		if (srec.pack_registry_root != 0 &&
		    srec.pack_registry_root != tmp_->sb.pack_registry_root) {
			tessera_btree_t *t = tessera_btree_open(&tmp_->meta_bio,
			    srec.pack_registry_root, /*tree_kind*/ 1,
			    /*key*/ 16, /*value*/ TESSERA_REGISTRY_ENTRY_SIZE);
			if (t != NULL) {
				(void)tessera_btree_walk_nodes(t,
				    meta_mark_visitor, &mctx);
				tessera_btree_close(t);
			}
		}
		if (srec.free_extent_root != 0 &&
		    srec.free_extent_root != tmp_->sb.free_extent_root) {
			tessera_btree_t *t = tessera_btree_open(&tmp_->meta_bio,
			    srec.free_extent_root, /*tree_kind*/ 2,
			    /*key*/ 8, /*value*/ 8);
			if (t != NULL) {
				(void)tessera_btree_walk_nodes(t,
				    meta_mark_visitor, &mctx);
				tessera_btree_close(t);
			}
		}
		if (tessera_btree_cursor_next(sc) != TESSERA_OK) break;
	}
	if (sc != NULL) tessera_btree_cursor_free(sc);
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
	/* v2.6 B.2: dirent log records re-create the in-memory log
	 * for replay. They live alongside ROOT_UPDATE records in the
	 * journal; the dirent record handler returns 1 on hit so the
	 * SB-commit path below is skipped. */
	if (tessera_replay_dirent_record(rc->tmp_, hdr, body))
		return (0);
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
	rc->tmp_->sb.snapshots_root     = rec.snapshots_root;
	rc->tmp_->sb.snapshots_gen      = rec.snapshots_gen;
	rc->tmp_->sb.next_inode_no      = rec.next_inode_no;
	rc->tmp_->sb.meta_reserve_bump  = rec.meta_reserve_bump;
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
	/* v2 step-2b: check the in-memory pending-manifest cache first.
	 * Manifests written via publish_manifest land here without
	 * touching disk until flush; readers in the same flush window
	 * pick them up from RAM. Returns a freshly malloc'd copy of the
	 * cached bytes — caller frees as if from the disk path. */
	if (tessera_fs_pending_manifest_lookup(tmp_, hash,
	    out_buf, out_len) != 0)
		return (0);
	if (tmp_->pack_registry_tree == NULL) return (ENOENT);

	/* Tier B: bytes cache hit returns the blob with no disk I/O. */
	if (tessera_cas_byte_lookup(&tmp_->cas_cache, hash,
	    out_buf, out_len))
		return (0);

	/* Tier A: location cache hit — skip the O(N) linear scan of the
	 * pack registry, jump straight to bread + parse. */
	{
		struct tessera_cas_loc_snap snap;
		if (tessera_cas_loc_lookup(&tmp_->cas_cache, hash, &snap)) {
			tessera_pack_extent_t *exts = NULL;
			uint32_t nexts = 0;
			tessera_pack_extent_t inline_one[4];
			uint64_t total_sectors = snap.total_sectors;
			int need_free_exts = 0;

			if (snap.n_extents <= 4) {
				for (uint32_t i = 0; i < snap.n_extents; i++)
					inline_one[i] = snap.extents[i];
				exts = inline_one;
				nexts = snap.n_extents;
			} else {
				/* Multi-extent (PEL) pack — fetch the registry
				 * entry by pack_id (O(log N)) and resolve
				 * extents. Still much cheaper than scanning. */
				uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
				if (tessera_btree_get(tmp_->pack_registry_tree,
				    snap.pack_id, reg_value) != TESSERA_OK)
					goto cas_fast_miss; /* stale entry */
				tessera_registry_entry_t re;
				if (tessera_decode_registry_entry(reg_value,
				    &re) != TESSERA_OK)
					goto cas_fast_miss;
				if (tessera_fs_pack_extents_resolve(tmp_, &re,
				    &exts, &nexts) != 0)
					goto cas_fast_miss;
				need_free_exts = 1;
				total_sectors = re.length_sectors;
			}

			if (total_sectors == 0 ||
			    total_sectors > TESSERA_FETCH_PACK_MAX_SECTORS) {
				if (need_free_exts) free(exts, M_TESSERA);
				goto cas_fast_miss;
			}
			const size_t pack_len =
			    (size_t)total_sectors * TESSERA_SECTOR_SIZE;
			uint8_t *packbuf = malloc(pack_len, M_TESSERA, M_NOWAIT);
			if (packbuf == NULL) {
				if (need_free_exts) free(exts, M_TESSERA);
				goto cas_fast_miss;
			}
			int read_ok = 1;
			uint64_t cursor = 0;
			for (uint32_t e = 0; e < nexts && read_ok; e++) {
				for (uint64_t i = 0;
				    i < exts[e].length_sectors; i++) {
					struct buf *bp = NULL;
					int err = bread(tmp_->devvp,
					    (exts[e].start_sector + i) *
					        btodb(TESSERA_SECTOR_SIZE),
					    TESSERA_SECTOR_SIZE,
					    tmp_->bio_ctx.cred ?
					        tmp_->bio_ctx.cred : NOCRED,
					    &bp);
					if (err != 0) {
						if (bp != NULL) brelse(bp);
						read_ok = 0;
						break;
					}
					memcpy(packbuf +
					    (cursor + i) * TESSERA_SECTOR_SIZE,
					    bp->b_data, TESSERA_SECTOR_SIZE);
					brelse(bp);
				}
				cursor += exts[e].length_sectors;
			}
			if (need_free_exts) free(exts, M_TESSERA);
			if (!read_ok) {
				free(packbuf, M_TESSERA);
				goto cas_fast_miss;
			}
			tessera_pack_reader_t *pr =
			    tessera_pack_open(packbuf, pack_len);
			if (pr == NULL) {
				free(packbuf, M_TESSERA);
				goto cas_fast_miss;
			}
			const uint8_t *bytes = NULL;
			uint32_t blen = 0;
			if (tessera_pack_lookup(pr, hash, &bytes, &blen)
			    == TESSERA_OK) {
				uint8_t *copy = malloc(blen, M_TESSERA, M_WAITOK);
				memcpy(copy, bytes, blen);
				*out_buf = copy;
				*out_len = blen;
				/* Stash bytes for next time (eligibility
				 * checked in cas_byte_insert). */
				tessera_cas_byte_insert(&tmp_->cas_cache,
				    hash, bytes, blen);
				tessera_pack_close(pr);
				free(packbuf, M_TESSERA);
				return (0);
			}
			/* Cached pack_id no longer contains this hash —
			 * stale entry (e.g. post-repack). Fall through to
			 * the slow scan; stage 4's invalidation should
			 * normally prevent this. */
			tessera_pack_close(pr);
			free(packbuf, M_TESSERA);
		}
	}
cas_fast_miss:
	;
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

		tessera_pack_extent_t *exts = NULL;
		uint32_t nexts = 0;
		if (tessera_fs_pack_extents_resolve(tmp_, &re, &exts, &nexts)
		    != 0) {
			free(packbuf, M_TESSERA);
			goto next_pack;
		}

		/* Snapshot the first 4 extents for the CAS-cache insert
		 * (in case this is the pack containing our hash).
		 * Multi-extent packs beyond 4 fall back to the resolver. */
		tessera_pack_extent_t snap_exts[4];
		uint8_t snap_n = (nexts <= 4) ? (uint8_t)nexts : (uint8_t)0xFF;
		if (snap_n <= 4) {
			for (uint32_t i = 0; i < nexts; i++)
				snap_exts[i] = exts[i];
		}

		int read_ok = 1;
		uint64_t cursor = 0;
		for (uint32_t e = 0; e < nexts && read_ok; e++) {
			for (uint64_t i = 0; i < exts[e].length_sectors; i++) {
				struct buf *bp = NULL;
				int err = bread(tmp_->devvp,
				    (exts[e].start_sector + i) *
				        btodb(TESSERA_SECTOR_SIZE),
				    TESSERA_SECTOR_SIZE,
				    tmp_->bio_ctx.cred ?
				        tmp_->bio_ctx.cred : NOCRED,
				    &bp);
				if (err != 0) {
					if (bp != NULL) brelse(bp);
					read_ok = 0;
					break;
				}
				memcpy(packbuf +
				    (cursor + i) * TESSERA_SECTOR_SIZE,
				    bp->b_data, TESSERA_SECTOR_SIZE);
				brelse(bp);
			}
			cursor += exts[e].length_sectors;
		}
		free(exts, M_TESSERA);
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
				/* Stash bytes for next time. */
				tessera_cas_byte_insert(&tmp_->cas_cache,
				    hash, bytes, blen);
				tessera_pack_close(pr);
				free(packbuf, M_TESSERA);
				/* Cache this hit so future fetches skip the
				 * scan. `key` is the pack_id (registry key). */
				tessera_cas_loc_insert(&tmp_->cas_cache, hash,
				    key, snap_exts, snap_n,
				    re.length_sectors);
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

	/* Recursively walk a manifest tree starting from `root`,
	 * pushing every transitively reachable manifest/blob hash
	 * onto the live set. Without this, a multi-level directory's
	 * leaf manifests, a DIRECTORY_2L outer's bucket manifests, or
	 * a CHUNK_TREE file's inner-tier manifests would be reclaimed
	 * as orphans on the next GC pass — the inode.manifest_hash
	 * keeps only the ROOT alive. depth caps recursion so a corrupt
	 * cycle can't loop forever. Inline lambda via nested macro for
	 * conciseness — pure C function follows. */
#define _GC_WALK_INODE_TREE(_tree) do {                                   \
	if ((_tree) == NULL) break;                                       \
	tessera_btree_cursor_t *_c = tessera_btree_seek_first(_tree);     \
	if (_c == NULL) break;                                            \
	for (;;) {                                                        \
		uint8_t _k[4];                                            \
		tessera_inode_record_t _ino;                              \
		if (tessera_btree_cursor_get(_c, _k, &_ino) != TESSERA_OK)\
			break;                                            \
		tessera_hash_t _stack[64];                                \
		int _sp = 0;                                              \
		if (!tessera_hash_is_null(_ino.manifest_hash)) {          \
			memcpy(_stack[_sp++], _ino.manifest_hash,         \
			    TESSERA_HASH_SIZE);                           \
		}                                                         \
		while (_sp > 0 && _sp < 64) {                             \
			tessera_hash_t _h;                                \
			memcpy(_h, _stack[--_sp], TESSERA_HASH_SIZE);     \
			_GC_PUSH_HASH(_h);                                \
			uint8_t  *_blob = NULL;                           \
			uint32_t _blen = 0;                               \
			if (tessera_fs_fetch_blob(tmp_, _h, &_blob,       \
			    &_blen) != 0) continue;                       \
			tessera_manifest_parser_t *_p =                   \
			    tessera_manifest_parse(_blob, _blen);         \
			if (_p == NULL) { free(_blob, M_TESSERA);         \
			    continue; }                                   \
			tessera_manifest_kind_t _k2 =                     \
			    tessera_manifest_parser_kind(_p);             \
			uint32_t _cnt =                                   \
			    tessera_manifest_parser_count(_p);            \
			if (_k2 == TESSERA_MFT_CHUNK_LIST) {              \
				for (uint32_t _i = 0; _i < _cnt; _i++) {  \
					tessera_chunk_record_t _cr;       \
					if (tessera_manifest_chunk_at(_p, \
					    _i, &_cr) == TESSERA_OK)      \
						_GC_PUSH_HASH(_cr.chunk_hash); \
				}                                         \
			} else if (_k2 == TESSERA_MFT_CHUNK_TREE) {       \
				for (uint32_t _i = 0;                     \
				    _i < _cnt && _sp < 63; _i++) {        \
					tessera_tree_record_t _tr;        \
					if (tessera_manifest_tree_at(_p,  \
					    _i, &_tr) == TESSERA_OK)      \
						memcpy(_stack[_sp++],     \
						    _tr.child_manifest_hash, \
						    TESSERA_HASH_SIZE);   \
				}                                         \
			} else if (_k2 == TESSERA_MFT_DIRECTORY_2L) {     \
				for (uint32_t _i = 0;                     \
				    _i < _cnt && _sp < 63; _i++) {        \
					tessera_dir_bucket_record_t _br;  \
					if (tessera_manifest_dir_bucket_at(\
					    _p, _i, &_br) == TESSERA_OK)  \
						memcpy(_stack[_sp++],     \
						    _br.bucket_manifest_hash, \
						    TESSERA_HASH_SIZE);   \
				}                                         \
			} else if (_k2 == TESSERA_MFT_DIRECTORY_BTREE) {  \
				int _lf;                                  \
				const uint8_t *_body;                     \
				size_t _bblen;                            \
				uint32_t _bcnt;                           \
				if (tessera_fs_dir_btree_decode(_blob,    \
				    _blen, &_lf, &_body, &_bblen, &_bcnt) \
				    == 0 && !_lf) {                       \
					size_t _off = 0;                  \
					for (uint32_t _i = 0;             \
					    _i < _bcnt && _sp < 63;       \
					    _i++) {                       \
						if (_off + 8 +            \
						    TESSERA_HASH_SIZE >   \
						    _bblen) break;        \
						memcpy(_stack[_sp++],     \
						    _body + _off + 8,     \
						    TESSERA_HASH_SIZE);   \
						_off += 8 +               \
						    TESSERA_HASH_SIZE;    \
					}                                 \
				}                                         \
			}                                                 \
			tessera_manifest_parser_free(_p);                 \
			free(_blob, M_TESSERA);                           \
		}                                                         \
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
	struct dead {
		uint8_t  pack_id[16];
		uint64_t start;             /* contig start, OR PEL sector if multi */
		uint64_t len;               /* total length (sum of extents) */
		uint32_t flags;             /* MULTI_EXTENT bit drives the free path */
	};
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
			tessera_pack_extent_t *exts = NULL;
			uint32_t nexts = 0;
			int ok = (tessera_fs_pack_extents_resolve(tmp_, &re,
			    &exts, &nexts) == 0);
			uint64_t cursor = 0;
			for (uint32_t e = 0; e < nexts && ok; e++) {
				for (uint64_t i = 0; i < exts[e].length_sectors;
				     i++) {
					struct buf *bp = NULL;
					if (bread(tmp_->devvp,
					    (exts[e].start_sector + i) *
					        btodb(TESSERA_SECTOR_SIZE),
					    TESSERA_SECTOR_SIZE,
					    tmp_->bio_ctx.cred ?
					        tmp_->bio_ctx.cred : NOCRED,
					    &bp) != 0) {
						if (bp) brelse(bp);
						ok = 0; break;
					}
					memcpy(packbuf +
					    (cursor + i) * TESSERA_SECTOR_SIZE,
					    bp->b_data, TESSERA_SECTOR_SIZE);
					brelse(bp);
				}
				cursor += exts[e].length_sectors;
			}
			free(exts, M_TESSERA);
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
				deads[dead_count].flags = re.flags;
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
			tessera_cas_invalidate_pack(&tmp_->cas_cache,
			    deads[i].pack_id);
		}
		if ((deads[i].flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) == 0) {
			(void)tessera_extent_free(tmp_->extent_alloc,
			    deads[i].start, deads[i].len);
		} else {
			/* Multi-extent: read the PEL, free each extent, then
			 * free the PEL sector itself. */
			tessera_registry_entry_t fake;
			memset(&fake, 0, sizeof fake);
			fake.start_sector   = deads[i].start;
			fake.length_sectors = deads[i].len;
			fake.flags          = deads[i].flags;
			tessera_pack_extent_t *exts = NULL;
			uint32_t nexts = 0;
			if (tessera_fs_pack_extents_resolve(tmp_, &fake,
			    &exts, &nexts) == 0) {
				for (uint32_t e = 0; e < nexts; e++)
					(void)tessera_extent_free(
					    tmp_->extent_alloc,
					    exts[e].start_sector,
					    exts[e].length_sectors);
			}
			free(exts, M_TESSERA);
			/* Walk + free the PEL chain (one or more sectors). */
			uint64_t pel_s = deads[i].start;
			for (int d = 0; d < 64 && pel_s != 0; d++) {
				struct buf *bp = NULL;
				uint64_t next = 0;
				if (bread(tmp_->devvp,
				    pel_s * btodb(TESSERA_SECTOR_SIZE),
				    TESSERA_SECTOR_SIZE,
				    tmp_->bio_ctx.cred ?
				        tmp_->bio_ctx.cred : NOCRED,
				    &bp) == 0) {
					tessera_pack_extent_list_t pel;
					if (tessera_decode_pack_extent_list(
					    (const uint8_t *)bp->b_data, &pel)
					    == TESSERA_OK)
						next = pel.next_pel_sector;
					brelse(bp);
				}
				(void)tessera_extent_free(tmp_->extent_alloc,
				    pel_s, 1);
				pel_s = next;
			}
		}
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
tessera_fs_publish_manifest_owned_ex(struct tessera_mount *tmp_,
                                  const uint8_t *manifest_bytes, size_t mlen,
                                  tessera_hash_t out_hash,
                                  uint32_t owner_inode_no,
                                  int known_new)
{
	if (tmp_->pack_registry_tree == NULL || tmp_->extent_alloc == NULL)
		return (EROFS);

	tessera_sha256(manifest_bytes, mlen, out_hash);

	/* Publish-cache shortcut (publish_dedup): pack_id is derived
	 * from the manifest hash, so identical content lands at the same
	 * pack_id. If the registry already contains an entry, the pack
	 * is on disk; nothing to do.
	 *
	 * Callers that just built this content from scratch (BTREE leaf
	 * + inner publishes during dirent ops) pass known_new=1 to skip
	 * this pre-check entirely. The pending-manifest cache below
	 * already dedups by hash, and pack_registry's btree_put on
	 * commit_sb is idempotent for identical pack_id; the only thing
	 * the pre-check saves is a cache entry on a true content
	 * collision, which doesn't happen for fresh-built content.
	 *
	 * The pre-check ran a btree_get on pack_registry per publish,
	 * which descends a disk-backed tree (multiple kbio_reads). With
	 * the BTREE directory landing 2-3 publishes per dirent op, that
	 * was the dominant per-op cost. */
	if (!known_new) {
		uint8_t pack_id_local[16];
		memcpy(pack_id_local, out_hash, 16);
		uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
		if (tessera_btree_get(tmp_->pack_registry_tree,
		    pack_id_local, reg_value) == TESSERA_OK) {
			tessera_stat_publish_dedup_manifest++;
			return (0);
		}
	}

	/* v2 step-2b extension: defer the disk write. Cache the bytes
	 * keyed by hash; fetch_blob consults the cache before scanning
	 * the registry. Drained at flush time. Skipped (write-through)
	 * when the cache isn't initialised — mount-time GC etc.
	 * owner_inode_no enables supersession: subsequent publishes for
	 * the same owner drop the older pending bytes. */
	if (tmp_->dirty_init &&
	    tmp_->pending_manifest_bytes <
	        TESSERA_PENDING_MANIFEST_BYTES_MAX) {
		return (tessera_fs_pending_manifest_put(tmp_, out_hash,
		    manifest_bytes, (uint32_t)mlen, owner_inode_no));
	}
	return (tessera_fs_publish_manifest_to_disk(tmp_, manifest_bytes,
	    mlen, out_hash));
}

/* Original signature — caller has no logical-owner context (e.g.
 * bucket manifests, internal helpers). Equivalent to
 * publish_manifest_owned(..., 0). */
static int
tessera_fs_publish_manifest(struct tessera_mount *tmp_,
                            const uint8_t *manifest_bytes, size_t mlen,
                            tessera_hash_t out_hash)
{
	return (tessera_fs_publish_manifest_owned_ex(tmp_, manifest_bytes,
	    mlen, out_hash, /*owner_inode_no=*/0, /*known_new=*/0));
}

/* Default-arity wrapper — preserves "old" callers' behaviour
 * (pack_registry pre-check enabled). */
static int
tessera_fs_publish_manifest_owned(struct tessera_mount *tmp_,
                                  const uint8_t *manifest_bytes, size_t mlen,
                                  tessera_hash_t out_hash,
                                  uint32_t owner_inode_no)
{
	return (tessera_fs_publish_manifest_owned_ex(tmp_, manifest_bytes,
	    mlen, out_hash, owner_inode_no, /*known_new=*/0));
}

/* Fast-path variant for callers that just built `manifest_bytes`
 * from scratch and KNOW the content is unique (hash collisions are
 * cryptographically negligible at our scale). Skips the
 * pack_registry btree_get pre-check that dominated per-op cost in
 * the BTREE directory hot path. See the comment in
 * tessera_fs_publish_manifest_owned_ex for the safety argument. */
static int
tessera_fs_publish_manifest_owned_known_new(struct tessera_mount *tmp_,
                                            const uint8_t *manifest_bytes,
                                            size_t mlen,
                                            tessera_hash_t out_hash,
                                            uint32_t owner_inode_no)
{
	return (tessera_fs_publish_manifest_owned_ex(tmp_, manifest_bytes,
	    mlen, out_hash, owner_inode_no, /*known_new=*/1));
}

/* Result of tessera_fs_pack_alloc_and_write — the bits the caller
 * stamps onto a fresh tessera_registry_entry_t. */
struct tessera_pack_alloc_result {
	uint64_t start_sector;     /* contig start, OR PEL sector if multi */
	uint64_t length_sectors;   /* logical (sum of extents) */
	uint32_t flags;            /* SEALED + (MULTI_EXTENT if applicable) */
};

/*
 * Allocate data-zone space for `pack_bytes` (n_sectors) and write it
 * out. Tries a single contiguous allocation first — that's the fast
 * path that matches v1/v2 behaviour. If the data zone is fragmented
 * enough that a contiguous run isn't available, falls back to a
 * multi-extent (gang-block-style) layout: the pack body is split
 * across N runs and a "pack extent list" (PEL) sector indexes them.
 *
 * The registry entry is left to the caller to assemble; we just
 * report (start, length, flags).
 *
 * On any error mid-write, allocated extents are returned to the free
 * set so the caller doesn't have to clean up.
 */
static int
tessera_fs_pack_alloc_and_write(struct tessera_mount *tmp_,
                                const uint8_t *pack_bytes,
                                uint64_t n_sectors,
                                struct tessera_pack_alloc_result *out)
{
	if (tmp_->extent_alloc == NULL) return (EROFS);

	/* Fast path — single contiguous allocation. Skipped when the
	 * force_multi_extent debug knob is set, so tests can
	 * deterministically create MULTI_EXTENT packs. */
	uint64_t contig_start = 0;
	int r = tessera_force_multi_extent ? TESSERA_ENOSPC :
	    tessera_extent_alloc(tmp_->extent_alloc, n_sectors,
	    &contig_start);
	if (r == TESSERA_OK) {
		for (uint64_t i = 0; i < n_sectors; i++) {
			if (tessera_kbio_write(&tmp_->bio_ctx,
			    contig_start + i,
			    pack_bytes + i * TESSERA_SECTOR_SIZE) != 0) {
				(void)tessera_extent_free(tmp_->extent_alloc,
				    contig_start, n_sectors);
				return (EIO);
			}
		}
		out->start_sector   = contig_start;
		out->length_sectors = n_sectors;
		out->flags          = TESSERA_REGISTRY_FLAG_SEALED;
		return (0);
	}
	if (r != TESSERA_ENOSPC) return (EIO);

	/* Gang fallback with PEL chaining. When dust fragmentation makes
	 * a single PEL's 253-extent cap insufficient, allocate multiple
	 * PELs linked via next_pel_sector. Each iteration of the outer
	 * loop fills one PEL with up to PEL_MAX_EXTENTS extents covering
	 * as much of the remaining n_sectors as possible.
	 *
	 * Heap-alloc the per-iteration scratch (starts/lengths/pel/buf) —
	 * keeping them on the stack would push the helper's frame to ~12
	 * KiB even on the fast path, blowing FreeBSD's 16 KiB kernel
	 * stack from any deep call chain. */
	uint64_t *starts  = malloc(TESSERA_PEL_MAX_EXTENTS * sizeof *starts,
	    M_TESSERA, M_WAITOK);
	uint64_t *lengths = malloc(TESSERA_PEL_MAX_EXTENTS * sizeof *lengths,
	    M_TESSERA, M_WAITOK);
	tessera_pack_extent_list_t *pel = malloc(sizeof *pel, M_TESSERA,
	    M_WAITOK);
	uint8_t *pel_buf  = malloc(TESSERA_SECTOR_SIZE, M_TESSERA, M_WAITOK);

	/* Track every allocation we've made so we can roll back cleanly
	 * on any mid-stream failure. all_starts/all_lengths grows by
	 * doubling. */
	uint32_t all_cap = TESSERA_PEL_MAX_EXTENTS;
	uint32_t all_cnt = 0;
	uint64_t *all_starts  = malloc(all_cap * sizeof *all_starts,
	    M_TESSERA, M_WAITOK);
	uint64_t *all_lengths = malloc(all_cap * sizeof *all_lengths,
	    M_TESSERA, M_WAITOK);
	uint32_t pel_chain_cap = 16;
	uint32_t pel_chain_cnt = 0;
	uint64_t *pel_chain = malloc(pel_chain_cap * sizeof *pel_chain,
	    M_TESSERA, M_WAITOK);

#define _ROLLBACK_AND_RETURN(_rc) do {                                       \
	for (uint32_t _i = 0; _i < all_cnt; _i++)                            \
		(void)tessera_extent_free(tmp_->extent_alloc,                \
		    all_starts[_i], all_lengths[_i]);                        \
	for (uint32_t _i = 0; _i < pel_chain_cnt; _i++)                      \
		(void)tessera_extent_free(tmp_->extent_alloc,                \
		    pel_chain[_i], 1);                                       \
	free(pel_buf, M_TESSERA); free(pel, M_TESSERA);                      \
	free(lengths, M_TESSERA); free(starts, M_TESSERA);                   \
	free(all_starts, M_TESSERA); free(all_lengths, M_TESSERA);           \
	free(pel_chain, M_TESSERA);                                          \
	return (_rc);                                                        \
} while (0)

	uint64_t remaining = n_sectors;
	uint64_t data_cursor = 0;     /* offset in pack_bytes of next byte to write */
	uint64_t head_pel = 0;
	uint64_t prev_pel = 0;

	while (remaining > 0) {
		/* Allocate this PEL's sector FIRST. If we did this AFTER the
		 * data extents, alloc_multi_partial would happily grab every
		 * last free sector for data and leave zero for the PEL —
		 * which then ENOSPC's a workload that has plenty of contig
		 * space for data but ran the allocator down to the last
		 * sector. Reserving the 1-sector PEL up-front is symmetric
		 * with the contig fast-path's "allocate then write" order. */
		uint64_t pel_sector = 0;
		if (tessera_extent_alloc(tmp_->extent_alloc, 1, &pel_sector)
		    != TESSERA_OK)
			_ROLLBACK_AND_RETURN(ENOSPC);
		if (pel_chain_cnt == pel_chain_cap) {
			pel_chain_cap *= 2;
			uint64_t *gp = malloc(pel_chain_cap * sizeof *gp,
			    M_TESSERA, M_WAITOK);
			memcpy(gp, pel_chain, pel_chain_cnt * sizeof *gp);
			free(pel_chain, M_TESSERA);
			pel_chain = gp;
		}
		pel_chain[pel_chain_cnt++] = pel_sector;
		if (head_pel == 0) head_pel = pel_sector;

		uint32_t count = 0;
		uint64_t filled = 0;
		r = tessera_extent_alloc_multi_partial(tmp_->extent_alloc,
		    remaining, TESSERA_PEL_MAX_EXTENTS,
		    starts, lengths, &count, &filled);
		if (r != TESSERA_OK) {
			printf("tessera_fs: pack alloc — %llu of %llu sectors "
			    "remaining: extent allocator exhausted (r=%d)\n",
			    (unsigned long long)remaining,
			    (unsigned long long)n_sectors, r);
			_ROLLBACK_AND_RETURN(ENOSPC);
		}

		/* Record data allocations for rollback. */
		while (all_cnt + count > all_cap) {
			all_cap *= 2;
			uint64_t *gs = malloc(all_cap * sizeof *gs,
			    M_TESSERA, M_WAITOK);
			uint64_t *gl = malloc(all_cap * sizeof *gl,
			    M_TESSERA, M_WAITOK);
			memcpy(gs, all_starts,  all_cnt * sizeof *gs);
			memcpy(gl, all_lengths, all_cnt * sizeof *gl);
			free(all_starts, M_TESSERA);
			free(all_lengths, M_TESSERA);
			all_starts  = gs;
			all_lengths = gl;
		}
		for (uint32_t i = 0; i < count; i++) {
			all_starts[all_cnt]  = starts[i];
			all_lengths[all_cnt] = lengths[i];
			all_cnt++;
		}

		/* Write data extents for this PEL. */
		for (uint32_t i = 0; i < count; i++) {
			for (uint64_t j = 0; j < lengths[i]; j++) {
				if (tessera_kbio_write(&tmp_->bio_ctx,
				    starts[i] + j,
				    pack_bytes + (data_cursor + j) *
				        TESSERA_SECTOR_SIZE) != 0)
					_ROLLBACK_AND_RETURN(EIO);
			}
			data_cursor += lengths[i];
		}

		/* Build + write this PEL (next_pel_sector starts as 0; if a
		 * later iteration needs a continuation we'll rewrite). */
		memset(pel, 0, sizeof *pel);
		pel->magic           = TESSERA_PEL_MAGIC;
		pel->version         = 1;
		pel->extent_count    = count;
		/* total_length only meaningful in head PEL; set there. */
		pel->total_length    = (head_pel == pel_sector) ? n_sectors : 0;
		pel->next_pel_sector = 0;
		for (uint32_t i = 0; i < count; i++) {
			pel->extents[i].start_sector   = starts[i];
			pel->extents[i].length_sectors = lengths[i];
		}
		if (tessera_encode_pack_extent_list(pel, pel_buf) != TESSERA_OK
		    || tessera_kbio_write(&tmp_->bio_ctx, pel_sector, pel_buf)
		        != 0)
			_ROLLBACK_AND_RETURN(EIO);

		/* If this isn't the first PEL, link prev → this. */
		if (prev_pel != 0) {
			struct buf *bp = NULL;
			if (bread(tmp_->devvp,
			    prev_pel * btodb(TESSERA_SECTOR_SIZE),
			    TESSERA_SECTOR_SIZE,
			    tmp_->bio_ctx.cred ?
			        tmp_->bio_ctx.cred : NOCRED, &bp) != 0)
				_ROLLBACK_AND_RETURN(EIO);
			tessera_pack_extent_list_t prev;
			if (tessera_decode_pack_extent_list(
			    (const uint8_t *)bp->b_data, &prev) != TESSERA_OK) {
				brelse(bp);
				_ROLLBACK_AND_RETURN(EIO);
			}
			brelse(bp);
			prev.next_pel_sector = pel_sector;
			if (tessera_encode_pack_extent_list(&prev, pel_buf)
			    != TESSERA_OK ||
			    tessera_kbio_write(&tmp_->bio_ctx, prev_pel,
			    pel_buf) != 0)
				_ROLLBACK_AND_RETURN(EIO);
		}
		prev_pel = pel_sector;
		remaining -= filled;
	}

	printf("tessera_fs: pack — %llu sectors written across %u extents "
	    "in %u-PEL chain (head at sector %llu)\n",
	    (unsigned long long)n_sectors, all_cnt, pel_chain_cnt,
	    (unsigned long long)head_pel);
	out->start_sector   = head_pel;
	out->length_sectors = n_sectors;
	out->flags = TESSERA_REGISTRY_FLAG_SEALED |
	             TESSERA_REGISTRY_FLAG_MULTI_EXTENT;
	tmp_->multi_extent_pack_count++;
	free(pel_buf, M_TESSERA); free(pel, M_TESSERA);
	free(lengths, M_TESSERA); free(starts, M_TESSERA);
	free(all_starts, M_TESSERA); free(all_lengths, M_TESSERA);
	free(pel_chain, M_TESSERA);
	return (0);
#undef _ROLLBACK_AND_RETURN
}

/*
 * Resolve a registry entry to its extent list. Single-extent packs
 * (no MULTI_EXTENT flag) get a one-element list constructed inline;
 * multi-extent packs read the PEL sector. extents_out must hold up
 * to TESSERA_PEL_MAX_EXTENTS entries.
 */
static int
tessera_fs_pack_extents_resolve(struct tessera_mount *tmp_,
                                const tessera_registry_entry_t *re,
                                tessera_pack_extent_t **out_extents,
                                uint32_t *out_count)
{
	if ((re->flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) == 0) {
		tessera_pack_extent_t *single = malloc(sizeof *single,
		    M_TESSERA, M_WAITOK);
		single[0].start_sector   = re->start_sector;
		single[0].length_sectors = re->length_sectors;
		*out_extents = single;
		*out_count   = 1;
		return (0);
	}

	/* PEL chain walk. Each PEL holds up to TESSERA_PEL_MAX_EXTENTS;
	 * `next_pel_sector` (0 = end of chain) lets a single pack span
	 * arbitrarily many extents when the data zone is dust-fragmented.
	 * Grow `result` by doubling so we don't N×realloc on long chains. */
	uint32_t cap = TESSERA_PEL_MAX_EXTENTS;
	uint32_t cnt = 0;
	tessera_pack_extent_t *result = malloc(cap * sizeof *result,
	    M_TESSERA, M_WAITOK);

	uint64_t cur_sector = re->start_sector;
	for (int depth = 0; depth < 64; depth++) {  /* depth cap */
		struct buf *bp = NULL;
		int err = bread(tmp_->devvp,
		    cur_sector * btodb(TESSERA_SECTOR_SIZE),
		    TESSERA_SECTOR_SIZE,
		    tmp_->bio_ctx.cred ? tmp_->bio_ctx.cred : NOCRED, &bp);
		if (err != 0) {
			if (bp != NULL) brelse(bp);
			free(result, M_TESSERA);
			return (EIO);
		}
		tessera_pack_extent_list_t pel;
		int r = tessera_decode_pack_extent_list(
		    (const uint8_t *)bp->b_data, &pel);
		brelse(bp);
		if (r != TESSERA_OK) {
			printf("tessera_fs: pack extent list at sector %llu — "
			    "decode failed: r=%d (chain depth=%d)\n",
			    (unsigned long long)cur_sector, r, depth);
			free(result, M_TESSERA);
			return (EIO);
		}
		if (cnt + pel.extent_count > cap) {
			while (cap < cnt + pel.extent_count) cap *= 2;
			tessera_pack_extent_t *grown = malloc(
			    cap * sizeof *grown, M_TESSERA, M_WAITOK);
			memcpy(grown, result, cnt * sizeof *result);
			free(result, M_TESSERA);
			result = grown;
		}
		for (uint32_t i = 0; i < pel.extent_count; i++)
			result[cnt++] = pel.extents[i];
		if (pel.next_pel_sector == 0) break;
		cur_sector = pel.next_pel_sector;
	}

	*out_extents = result;
	*out_count   = cnt;
	return (0);
}

/*
 * Repack a single multi-extent pack into a (preferably contiguous)
 * fresh allocation. Same `pack_id` — the registry update is a single
 * btree_put on the same key. Crash anywhere is safe: old extents +
 * old PEL stay on disk and registered until step 4 (the btree_put)
 * commits; if we crash before, the new copy is orphan that mount-
 * time GC will reclaim. After step 4, the old extents are
 * unreferenced; we free them in step 5.
 *
 *   1. Look up registry entry by pack_id.
 *   2. If not MULTI_EXTENT — no-op success.
 *   3. Resolve current extents, read pack body into a kernel buf.
 *   4. tessera_fs_pack_alloc_and_write writes the body to a fresh
 *      location and returns the new (start, length, flags). The
 *      contig-first fallback in that helper means a successful
 *      repack will be single-extent if any contig run fits, or a
 *      smaller-extent-count multi otherwise. Either way an
 *      improvement.
 *   5. btree_put the updated registry entry. **Commit point.**
 *   6. Free old extents (each entry from the old PEL) and the old
 *      PEL sector.
 *
 * Returns 0 on success or no-op-needed, errno otherwise. Sets
 * *out_was_repacked to 1 if a real repack happened (caller's stats).
 */
static int
tessera_fs_repack_one_pack(struct tessera_mount *tmp_,
                           const uint8_t pack_id[16],
                           int *out_was_repacked)
{
	if (out_was_repacked != NULL) *out_was_repacked = 0;
	if (tmp_->pack_registry_tree == NULL) return (EROFS);

	uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
	if (tessera_btree_get(tmp_->pack_registry_tree, pack_id, reg_value)
	    != TESSERA_OK)
		return (ENOENT);
	tessera_registry_entry_t re;
	if (tessera_decode_registry_entry(reg_value, &re) != TESSERA_OK)
		return (EIO);

	/* Only multi-extent packs are repack candidates. */
	if ((re.flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) == 0)
		return (0);

	/* Resolve the current extents — we'll need them for both reading
	 * the old body and freeing them at the end. */
	tessera_pack_extent_t *old_exts = NULL;
	uint32_t old_nexts = 0;
	if (tessera_fs_pack_extents_resolve(tmp_, &re, &old_exts, &old_nexts)
	    != 0)
		return (EIO);

	/* Materialise the pack body. */
	const size_t body_len =
	    (size_t)re.length_sectors * TESSERA_SECTOR_SIZE;
	uint8_t *body = malloc(body_len, M_TESSERA, M_WAITOK);
	int read_ok = 1;
	uint64_t cursor = 0;
	for (uint32_t e = 0; e < old_nexts && read_ok; e++) {
		for (uint64_t i = 0; i < old_exts[e].length_sectors; i++) {
			struct buf *bp = NULL;
			int err = bread(tmp_->devvp,
			    (old_exts[e].start_sector + i) *
			        btodb(TESSERA_SECTOR_SIZE),
			    TESSERA_SECTOR_SIZE,
			    tmp_->bio_ctx.cred ?
			        tmp_->bio_ctx.cred : NOCRED, &bp);
			if (err != 0) {
				if (bp != NULL) brelse(bp);
				read_ok = 0;
				break;
			}
			memcpy(body + (cursor + i) * TESSERA_SECTOR_SIZE,
			    bp->b_data, TESSERA_SECTOR_SIZE);
			brelse(bp);
		}
		cursor += old_exts[e].length_sectors;
	}
	if (!read_ok) {
		free(body, M_TESSERA);
		free(old_exts, M_TESSERA);
		return (EIO);
	}

	/* Allocate + write a fresh location. The helper prefers contig
	 * and falls back to multi only if contig fails — for repack this
	 * is exactly what we want. */
	struct tessera_pack_alloc_result pa;
	int wrt = tessera_fs_pack_alloc_and_write(tmp_, body,
	    re.length_sectors, &pa);
	free(body, M_TESSERA);
	if (wrt != 0) {
		/* Couldn't allocate space — leave old layout intact. */
		free(old_exts, M_TESSERA);
		return (wrt);
	}

	/* Commit point: same pack_id, new layout, preserved metadata. */
	tessera_registry_entry_t new_re = re;
	new_re.start_sector   = pa.start_sector;
	new_re.length_sectors = pa.length_sectors;
	new_re.flags          = pa.flags;
	uint8_t new_value[TESSERA_REGISTRY_ENTRY_SIZE];
	if (tessera_encode_registry_entry(&new_re, new_value) != TESSERA_OK) {
		/* Should not fail; defensive cleanup of the new copy. */
		if ((pa.flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) == 0) {
			(void)tessera_extent_free(tmp_->extent_alloc,
			    pa.start_sector, pa.length_sectors);
		} else {
			tessera_pack_extent_t *neexts = NULL;
			uint32_t nnexts = 0;
			if (tessera_fs_pack_extents_resolve(tmp_, &new_re,
			    &neexts, &nnexts) == 0) {
				for (uint32_t i = 0; i < nnexts; i++)
					(void)tessera_extent_free(
					    tmp_->extent_alloc,
					    neexts[i].start_sector,
					    neexts[i].length_sectors);
			}
			free(neexts, M_TESSERA);
			(void)tessera_extent_free(tmp_->extent_alloc,
			    pa.start_sector, 1);
		}
		free(old_exts, M_TESSERA);
		return (EIO);
	}
	uint64_t new_pack_root = tmp_->sb.pack_registry_root;
	if (tessera_btree_put(tmp_->pack_registry_tree, pack_id,
	    new_value, &new_pack_root) != TESSERA_OK) {
		free(old_exts, M_TESSERA);
		return (EIO);
	}
	tmp_->sb.pack_registry_root = new_pack_root;

	/* Same pack_id, new layout — invalidate any cache entries that
	 * still point at the OLD extents. They'd bread freed sectors. */
	tessera_cas_invalidate_pack(&tmp_->cas_cache, pack_id);

	/* Past the commit point — free the OLD extents + old PEL. */
	for (uint32_t e = 0; e < old_nexts; e++)
		(void)tessera_extent_free(tmp_->extent_alloc,
		    old_exts[e].start_sector, old_exts[e].length_sectors);
	/* Walk the OLD PEL chain and free each PEL sector. With chaining
	 * a single multi-extent pack can have multiple PELs linked via
	 * next_pel_sector; the original "1 sector" assumption was true
	 * only before chaining landed. */
	{
		uint64_t pel_s = re.start_sector;
		for (int d = 0; d < 64 && pel_s != 0; d++) {
			struct buf *bp = NULL;
			uint64_t next = 0;
			if (bread(tmp_->devvp,
			    pel_s * btodb(TESSERA_SECTOR_SIZE),
			    TESSERA_SECTOR_SIZE,
			    tmp_->bio_ctx.cred ?
			        tmp_->bio_ctx.cred : NOCRED,
			    &bp) == 0) {
				tessera_pack_extent_list_t pel;
				if (tessera_decode_pack_extent_list(
				    (const uint8_t *)bp->b_data, &pel)
				    == TESSERA_OK)
					next = pel.next_pel_sector;
				brelse(bp);
			}
			(void)tessera_extent_free(tmp_->extent_alloc,
			    pel_s, 1);
			pel_s = next;
		}
	}
	free(old_exts, M_TESSERA);

	/* Counter delta: this pack was MULTI_EXTENT before (we wouldn't
	 * have entered this helper otherwise). Decrement once for the
	 * OLD entry; pack_alloc_and_write already incremented for the
	 * NEW entry if it took the multi path. Net effect:
	 *   contig new → -1 (the desired drop)
	 *   multi  new → -1 + +1 = 0 (still fragmented, count unchanged) */
	if (tmp_->multi_extent_pack_count > 0)
		tmp_->multi_extent_pack_count--;

	printf("tessera_fs: repacked pack — was %u extents, now %s "
	    "(new start %llu, len %llu sectors)\n",
	    old_nexts,
	    (pa.flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) ? "multi" : "contig",
	    (unsigned long long)pa.start_sector,
	    (unsigned long long)pa.length_sectors);
	if (out_was_repacked != NULL) *out_was_repacked = 1;
	tessera_fs_mark_dirty(tmp_);
	return (0);
}

/*
 * Repack the first MULTI_EXTENT pack we find. Used by the
 * `kern.tessera.repack_one` sysctl for B1 testing — picks any
 * multi-extent pack rather than a specific id.
 */
static int tessera_fs_repack_first_multi(struct tessera_mount *tmp_);

/*
 * sysctl handler — write any non-zero value to trigger one repack.
 * Reads back as 0. Returns the errno from the repack attempt as the
 * sysctl write result so userspace can see it (`sysctl ... = 1` works,
 * a failure shows up as e.g. `sysctl: ...: No such file or directory`
 * for ENOENT).
 */
static int
tessera_sysctl_repack_one(SYSCTL_HANDLER_ARGS)
{
	int trigger = 0;
	int err = sysctl_handle_int(oidp, &trigger, 0, req);
	if (err != 0 || req->newptr == NULL) return (err);
	if (trigger == 0) return (0);
	if (tessera_singleton_mount == NULL) return (ENXIO);
	return (tessera_fs_repack_first_multi(tessera_singleton_mount));
}
SYSCTL_PROC(_kern_tessera, OID_AUTO, repack_one,
    CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_MPSAFE,
    NULL, 0, tessera_sysctl_repack_one, "I",
    "Write 1 to repack one MULTI_EXTENT pack on the active tessera mount");

/*
 * Bounded repack pass — B2 driver. Walks the pack_registry in tree
 * order; for each MULTI_EXTENT pack found, applies the B1 helper.
 * Bounded by both a pack-count cap and a wallclock-time cap.
 *
 * After each successful repack the cursor would be invalidated (B1
 * does a btree_put on the same key, plus may trigger meta-reserve
 * activity). We restart the walk via seek_first each time. That's
 * O(packs * MULTI_EXTENT_count) in the worst case, but
 * MULTI_EXTENT_count drops with every iteration — totals stay linear
 * in the work to do.
 *
 * We deliberately don't sort by extent_count (descending) here — that
 * would require reading the PEL sector for every multi-extent pack
 * just to choose ordering, which costs more I/O than the marginal
 * benefit of repacking the most fragmented first. Tree order suffices.
 */
static unsigned long tessera_repack_total_packs = 0;
static unsigned long tessera_repack_last_packs = 0;
static unsigned long tessera_repack_last_time_ms = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, repack_total_packs,
    CTLFLAG_RD, &tessera_repack_total_packs, 0,
    "Cumulative count of packs repacked since module load");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, repack_last_packs,
    CTLFLAG_RD, &tessera_repack_last_packs, 0,
    "Packs repacked in the most recent pass");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, repack_last_time_ms,
    CTLFLAG_RD, &tessera_repack_last_time_ms, 0,
    "Wallclock duration (ms) of the most recent repack pass");

static int
tessera_fs_repack_pass(struct tessera_mount *tmp_,
                       uint32_t max_packs, uint32_t max_time_ms,
                       uint32_t *out_repacked)
{
	if (tmp_->pack_registry_tree == NULL) return (EROFS);

	struct timeval tv0, tv1;
	getmicrotime(&tv0);
	uint32_t repacked = 0;

	while (repacked < max_packs) {
		getmicrotime(&tv1);
		uint64_t elapsed_ms = (uint64_t)(tv1.tv_sec - tv0.tv_sec) * 1000ULL +
		    ((uint64_t)tv1.tv_usec - (uint64_t)tv0.tv_usec) / 1000ULL;
		if (elapsed_ms >= max_time_ms) break;

		tessera_btree_cursor_t *c =
		    tessera_btree_seek_first(tmp_->pack_registry_tree);
		if (c == NULL) break;
		uint8_t found_key[16];
		int found = 0;
		for (;;) {
			uint8_t key[16];
			uint8_t value[TESSERA_REGISTRY_ENTRY_SIZE];
			if (tessera_btree_cursor_get(c, key, value) != TESSERA_OK)
				break;
			tessera_registry_entry_t re;
			if (tessera_decode_registry_entry(value, &re) == TESSERA_OK &&
			    (re.flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) != 0) {
				memcpy(found_key, key, 16);
				found = 1;
				break;
			}
			if (tessera_btree_cursor_next(c) != TESSERA_OK) break;
		}
		tessera_btree_cursor_free(c);
		if (!found) break;

		int was = 0;
		int err = tessera_fs_repack_one_pack(tmp_, found_key, &was);
		if (err != 0) {
			if (out_repacked != NULL) *out_repacked = repacked;
			tessera_repack_last_packs = repacked;
			return (err);
		}
		if (was) repacked++;
	}

	getmicrotime(&tv1);
	uint64_t elapsed_ms = (uint64_t)(tv1.tv_sec - tv0.tv_sec) * 1000ULL +
	    ((uint64_t)tv1.tv_usec - (uint64_t)tv0.tv_usec) / 1000ULL;
	tessera_repack_last_time_ms = (unsigned long)elapsed_ms;
	tessera_repack_last_packs = repacked;
	tessera_repack_total_packs += repacked;
	if (out_repacked != NULL) *out_repacked = repacked;
	printf("tessera_fs: repack pass — %u packs repacked in %lu ms\n",
	    repacked, (unsigned long)elapsed_ms);
	return (0);
}

/*
 * sysctl kern.tessera.repack_now — write a non-zero value (interpreted
 * as a max-packs budget; 0 → default 1000) to run a bounded repack
 * pass synchronously on the active mount. 30s wallclock cap. The
 * stats sysctls above expose the result.
 */
static int
tessera_sysctl_repack_now(SYSCTL_HANDLER_ARGS)
{
	int budget = 0;
	int err = sysctl_handle_int(oidp, &budget, 0, req);
	if (err != 0 || req->newptr == NULL) return (err);
	if (budget == 0) return (0);
	if (tessera_singleton_mount == NULL) return (ENXIO);
	uint32_t max_packs = (budget > 0) ? (uint32_t)budget : 1000u;
	uint32_t repacked = 0;
	return (tessera_fs_repack_pass(tessera_singleton_mount,
	    max_packs, 30000u, &repacked));
}
SYSCTL_PROC(_kern_tessera, OID_AUTO, repack_now,
    CTLTYPE_INT | CTLFLAG_RW | CTLFLAG_MPSAFE,
    NULL, 0, tessera_sysctl_repack_now, "I",
    "Write N to run a bounded repack pass (max N packs, 30s) on the active mount");

SYSCTL_INT(_kern_tessera, OID_AUTO, repack_threshold,
    CTLFLAG_RW, &tessera_repack_threshold, 0,
    "Background repack arms when MULTI_EXTENT pack count exceeds this");
SYSCTL_INT(_kern_tessera, OID_AUTO, repack_severe_threshold,
    CTLFLAG_RW, &tessera_repack_severe_threshold, 0,
    "Mount-time synchronous repack pass runs when count exceeds this");
SYSCTL_INT(_kern_tessera, OID_AUTO, repack_bg_max_packs,
    CTLFLAG_RW, &tessera_repack_bg_max_packs, 0,
    "Max packs per background repack invocation");
SYSCTL_INT(_kern_tessera, OID_AUTO, repack_bg_max_time_ms,
    CTLFLAG_RW, &tessera_repack_bg_max_time_ms, 0,
    "Max wallclock ms per background repack invocation");
SYSCTL_INT(_kern_tessera, OID_AUTO, repack_mount_max_packs,
    CTLFLAG_RW, &tessera_repack_mount_max_packs, 0,
    "Max packs in mount-time synchronous safety-net pass");
SYSCTL_INT(_kern_tessera, OID_AUTO, repack_mount_max_time_ms,
    CTLFLAG_RW, &tessera_repack_mount_max_time_ms, 0,
    "Max wallclock ms in mount-time synchronous safety-net pass");

/*
 * Background repack handler — runs on the kernel taskqueue when
 * mark_dirty observes multi_extent_pack_count > threshold. Bounded
 * (default 5 packs / 100 ms). If more work remains after the pass,
 * re-arms itself; the next mark_dirty would also re-arm regardless.
 * Bails out if the FS is unmounting.
 */
static void
tessera_fs_repack_task(void *ctx, int pending)
{
	(void)pending;
	struct tessera_mount *tmp_ = ctx;
	if (tmp_ == NULL || tmp_->flush_unmounting) return;
	if (tmp_->pack_registry_tree == NULL) return;

	uint32_t budget_packs = (uint32_t)tessera_repack_bg_max_packs;
	uint32_t budget_ms = (uint32_t)tessera_repack_bg_max_time_ms;
	if (budget_packs == 0) budget_packs = 5;
	if (budget_ms == 0) budget_ms = 100;
	uint32_t repacked = 0;
	(void)tessera_fs_repack_pass(tmp_, budget_packs, budget_ms,
	    &repacked);

	if (!tmp_->flush_unmounting &&
	    tmp_->multi_extent_pack_count >
	        (uint32_t)tessera_repack_threshold) {
		(void)taskqueue_enqueue(taskqueue_thread, &tmp_->repack_task);
	}
}

/*
 * Walk pack_registry once and count MULTI_EXTENT-flagged entries.
 * Used at mount time to seed multi_extent_pack_count and decide
 * whether to run the safety-net pass.
 */
static int
tessera_fs_count_multi_extent(struct tessera_mount *tmp_, uint32_t *out_count)
{
	*out_count = 0;
	if (tmp_->pack_registry_tree == NULL) return (0);
	tessera_btree_cursor_t *c =
	    tessera_btree_seek_first(tmp_->pack_registry_tree);
	if (c == NULL) return (0);
	uint32_t n = 0;
	for (;;) {
		uint8_t key[16];
		uint8_t value[TESSERA_REGISTRY_ENTRY_SIZE];
		if (tessera_btree_cursor_get(c, key, value) != TESSERA_OK)
			break;
		tessera_registry_entry_t re;
		if (tessera_decode_registry_entry(value, &re) == TESSERA_OK &&
		    (re.flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) != 0)
			n++;
		if (tessera_btree_cursor_next(c) != TESSERA_OK) break;
	}
	tessera_btree_cursor_free(c);
	*out_count = n;
	return (0);
}

static int
tessera_fs_repack_first_multi(struct tessera_mount *tmp_)
{
	if (tmp_->pack_registry_tree == NULL) return (EROFS);
	tessera_btree_cursor_t *c =
	    tessera_btree_seek_first(tmp_->pack_registry_tree);
	if (c == NULL) return (ENOENT);
	int err = ENOENT;
	for (;;) {
		uint8_t key[16];
		uint8_t value[TESSERA_REGISTRY_ENTRY_SIZE];
		if (tessera_btree_cursor_get(c, key, value) != TESSERA_OK)
			break;
		tessera_registry_entry_t re;
		if (tessera_decode_registry_entry(value, &re) == TESSERA_OK &&
		    (re.flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) != 0) {
			tessera_btree_cursor_free(c);
			int was = 0;
			err = tessera_fs_repack_one_pack(tmp_, key, &was);
			return (err);
		}
		if (tessera_btree_cursor_next(c) != TESSERA_OK) break;
	}
	tessera_btree_cursor_free(c);
	return (err);
}

static int
tessera_fs_publish_manifest_to_disk(struct tessera_mount *tmp_,
                                    const uint8_t *manifest_bytes,
                                    size_t mlen,
                                    const tessera_hash_t hash)
{
	uint8_t pack_id[16];
	memcpy(pack_id, hash, 16);

	tessera_pack_builder_t *pb = tessera_pack_begin(0, pack_id, 0);
	if (pb == NULL) return (ENOMEM);
	if (tessera_pack_add_blob(pb, hash, manifest_bytes,
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
	struct tessera_pack_alloc_result pa;
	int wrt = tessera_fs_pack_alloc_and_write(tmp_, pack_bytes,
	    n_sectors, &pa);
	free(pack_bytes, M_TESSERA);
	if (wrt != 0) return (wrt);

	tessera_registry_entry_t re;
	memset(&re, 0, sizeof re);
	memcpy(re.pack_id, pack_id, 16);
	re.start_sector    = pa.start_sector;
	re.length_sectors  = pa.length_sectors;
	re.blob_count      = 1;
	re.pack_kind       = 0;
	re.total_bytes     = pack_size;
	re.create_time     = 0;
	re.reachable_blobs = 1;
	re.flags           = pa.flags;
	uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
	if (tessera_encode_registry_entry(&re, reg_value) != TESSERA_OK)
		return (EIO);

	uint64_t new_pack_root = tmp_->sb.pack_registry_root;
	if (tessera_btree_put(tmp_->pack_registry_tree, pack_id, reg_value,
	    &new_pack_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.pack_registry_root = new_pack_root;

	/* CAS-cache insert for the manifest blob. Single-blob pack ⇒ one
	 * extent. Multi-extent layout (PEL) defers extent resolution to
	 * read time. */
	if ((pa.flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) == 0) {
		tessera_pack_extent_t one = {
			.start_sector   = pa.start_sector,
			.length_sectors = pa.length_sectors,
		};
		tessera_cas_loc_insert(&tmp_->cas_cache, hash, pack_id,
		    &one, 1, pa.length_sectors);
	} else {
		tessera_cas_loc_insert(&tmp_->cas_cache, hash, pack_id,
		    NULL, 0xFFu, pa.length_sectors);
	}
	return (0);
}

/* Multi-blob aggregation: bundle N small INLINE manifests into
 * ONE pack at drain time. Each tiny file would otherwise pay
 * ~16 KiB pack overhead (header + bloom + index + body alignment);
 * batching N together amortizes the overhead.
 *
 * pack_id is derived from the SHA256 of (sorted member hashes) so
 * republishing the same set lands at the same pack_id (rare but
 * free dedup). Per-blob dedup is checked by the caller before
 * adding to the batch.
 *
 * `entries` and `n` describe the batch. Each entry's bytes/len/hash
 * must be valid; ownership stays with the caller. Returns 0 on
 * success or an errno on failure (extents NOT freed by caller —
 * publish either commits or rolls back atomically).
 */
static int
tessera_fs_publish_manifests_batch(struct tessera_mount *tmp_,
                                   const struct tessera_aggr_entry *entries,
                                   uint32_t n)
{
	if (tmp_->pack_registry_tree == NULL || tmp_->extent_alloc == NULL)
		return (EROFS);
	if (n == 0) return (0);

	/* Derive pack_id = SHA256(concat of sorted member hashes).
	 * Sorting is required because pack_finalize sorts blobs by
	 * hash internally — a stable pack_id requires the same input
	 * order. */
	const size_t hashes_buf_len = (size_t)n * sizeof(tessera_hash_t);
	uint8_t *hashes_buf = malloc(hashes_buf_len, M_TESSERA, M_WAITOK);
	for (uint32_t i = 0; i < n; i++)
		memcpy(hashes_buf + i * sizeof(tessera_hash_t),
		    entries[i].hash, sizeof(tessera_hash_t));
	/* Insertion sort — n is bounded by aggregation_max_blobs (~64). */
	for (uint32_t i = 1; i < n; i++) {
		uint8_t key[32];
		memcpy(key, hashes_buf + i * 32, 32);
		uint32_t j = i;
		while (j > 0 && memcmp(hashes_buf + (j - 1) * 32, key, 32) > 0) {
			memcpy(hashes_buf + j * 32,
			    hashes_buf + (j - 1) * 32, 32);
			j--;
		}
		memcpy(hashes_buf + j * 32, key, 32);
	}
	tessera_hash_t agg_hash;
	tessera_sha256(hashes_buf, hashes_buf_len, agg_hash);
	free(hashes_buf, M_TESSERA);
	uint8_t pack_id[16];
	memcpy(pack_id, agg_hash, 16);

	/* If a pack with this same set already exists, skip the
	 * republish. */
	{
		uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
		if (tessera_btree_get(tmp_->pack_registry_tree, pack_id,
		    reg_value) == TESSERA_OK) {
			tessera_stat_publish_dedup_manifest++;
			return (0);
		}
	}

	tessera_pack_builder_t *pb = tessera_pack_begin(0 /* manifest pack */,
	    pack_id, 0);
	if (pb == NULL) return (ENOMEM);
	for (uint32_t i = 0; i < n; i++) {
		int ar = tessera_pack_add_blob(pb, entries[i].hash,
		    entries[i].bytes, entries[i].len,
		    TESSERA_BLOB_FLAG_MANIFEST);
		if (ar != TESSERA_OK) {
			/* TESSERA_EEXIST means the caller's batch had two
			 * entries with the same hash — caller must dedup
			 * before calling. Defensive: skip the dup. */
			if (ar == TESSERA_EEXIST) continue;
			tessera_pack_free(pb);
			return (EIO);
		}
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
	struct tessera_pack_alloc_result pa;
	int wrt = tessera_fs_pack_alloc_and_write(tmp_, pack_bytes,
	    n_sectors, &pa);
	free(pack_bytes, M_TESSERA);
	if (wrt != 0) return (wrt);

	tessera_registry_entry_t re;
	memset(&re, 0, sizeof re);
	memcpy(re.pack_id, pack_id, 16);
	re.start_sector    = pa.start_sector;
	re.length_sectors  = pa.length_sectors;
	re.blob_count      = n;
	re.pack_kind       = 0;
	re.total_bytes     = pack_size;
	re.create_time     = 0;
	re.reachable_blobs = n;
	re.flags           = pa.flags;
	uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
	if (tessera_encode_registry_entry(&re, reg_value) != TESSERA_OK)
		return (EIO);

	uint64_t new_pack_root = tmp_->sb.pack_registry_root;
	if (tessera_btree_put(tmp_->pack_registry_tree, pack_id, reg_value,
	    &new_pack_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.pack_registry_root = new_pack_root;

	/* Per-blob CAS-cache inserts. All blobs share the same physical
	 * extents — different hash keys, same location. */
	tessera_pack_extent_t cas_extents[4];
	uint8_t cas_n = 1;
	if ((pa.flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) == 0) {
		cas_extents[0].start_sector   = pa.start_sector;
		cas_extents[0].length_sectors = pa.length_sectors;
	} else {
		cas_n = 0xFFu;
	}
	for (uint32_t i = 0; i < n; i++) {
		tessera_cas_loc_insert(&tmp_->cas_cache, entries[i].hash,
		    pack_id, cas_extents, cas_n, pa.length_sectors);
	}

	tessera_stat_aggregation_packs++;
	tessera_stat_aggregation_blobs += n;
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

	/* Dedup chunks before adding to the pack: pack_finalize rejects
	 * duplicate hashes in the same pack (TESSERA_EEXIST), so a write
	 * with repeating content (e.g. all-zero file, or stress2's `rw`
	 * which writes uninitialized stack-buf data → all chunks have
	 * the same hash) would fail the publish entirely.
	 *
	 * The manifest's chunk_records can still repeat the hash —
	 * that's how multiple logical offsets map to a single shared
	 * blob. The pack body just stores each unique blob once. Use
	 * a small linear scan since n_chunks is bounded by the file's
	 * chunk fan-out (≤256). */
	for (uint32_t i = 0; i < n_chunks; i++) {
		int dup = 0;
		for (uint32_t j = 0; j < i; j++) {
			if (memcmp(chunks[j].hash, chunks[i].hash,
			    sizeof(tessera_hash_t)) == 0) {
				dup = 1; break;
			}
		}
		if (dup) continue;
		int ar = tessera_pack_add_blob(pb, chunks[i].hash,
		    chunks[i].bytes, chunks[i].len,
		    TESSERA_BLOB_FLAG_CHUNK);
		if (ar != TESSERA_OK) {
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
	struct tessera_pack_alloc_result pa;
	int wrt = tessera_fs_pack_alloc_and_write(tmp_, pack_bytes,
	    n_sectors, &pa);
	free(pack_bytes, M_TESSERA);
	if (wrt != 0) return (wrt);

	tessera_registry_entry_t re;
	memset(&re, 0, sizeof re);
	memcpy(re.pack_id, pack_id, 16);
	re.start_sector    = pa.start_sector;
	re.length_sectors  = pa.length_sectors;
	re.blob_count      = n_chunks + 1u;
	re.pack_kind       = 2;
	re.total_bytes     = pack_size;
	re.create_time     = 0;
	re.reachable_blobs = n_chunks + 1u;
	re.flags           = pa.flags;
	uint8_t reg_value[TESSERA_REGISTRY_ENTRY_SIZE];
	if (tessera_encode_registry_entry(&re, reg_value) != TESSERA_OK)
		return (EIO);

	uint64_t new_pack_root = tmp_->sb.pack_registry_root;
	if (tessera_btree_put(tmp_->pack_registry_tree, pack_id, reg_value,
	    &new_pack_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.pack_registry_root = new_pack_root;

	/* CAS-cache insert. We insert location entries for the manifest
	 * AND each chunk blob — every blob in this pack lives in the
	 * same physical extents, so they share the same location record
	 * structure (different hash key only). Without per-chunk
	 * inserts, the append-fast-path still pays an O(N) pack scan
	 * for the partial last chunk on every subsequent write. */
	tessera_pack_extent_t cas_extents[4];
	uint8_t cas_n = 1;
	if ((pa.flags & TESSERA_REGISTRY_FLAG_MULTI_EXTENT) == 0) {
		cas_extents[0].start_sector   = pa.start_sector;
		cas_extents[0].length_sectors = pa.length_sectors;
	} else {
		cas_n = 0xFFu;
	}
	tessera_cas_loc_insert(&tmp_->cas_cache, out_manifest_hash,
	    pack_id, cas_extents, cas_n, pa.length_sectors);
	for (uint32_t i = 0; i < n_chunks; i++) {
		tessera_cas_loc_insert(&tmp_->cas_cache, chunks[i].hash,
		    pack_id, cas_extents, cas_n, pa.length_sectors);
	}
	return (0);
}

/* ── v2 multi-level directory helpers ───────────────────────────── */

/* Iterate every dirent of a directory, transparently handling both
 * flat DIRECTORY and two-level DIRECTORY_2L manifests. Callback
 * returns 0 to continue, non-zero to abort the walk (returned by
 * dir_walk verbatim). */
static int
tessera_fs_dir_walk(struct tessera_mount *tmp_,
                    const tessera_hash_t dir_manifest_hash,
                    tessera_dirent_cb_t cb, void *ctx)
{
	if (cb == NULL) return (EINVAL);

	uint8_t  *blob = NULL;
	uint32_t  blob_len = 0;
	if (tessera_fs_fetch_blob(tmp_, dir_manifest_hash,
	    &blob, &blob_len) != 0) return (EIO);
	if (blob_len < 32) { free(blob, M_TESSERA); return (EIO); }

	tessera_manifest_parser_t *p = tessera_manifest_parse(blob, blob_len);
	if (p == NULL) { free(blob, M_TESSERA); return (EIO); }

	const tessera_manifest_kind_t k = tessera_manifest_parser_kind(p);
	int rc = 0;

	if (k == TESSERA_MFT_DIRECTORY) {
		const uint8_t *body = blob + 32;
		const size_t   blen = blob_len - 32;
		for (size_t off = 0; off + 10 <= blen; ) {
			uint64_t ch;
			uint16_t nl;
			memcpy(&ch, body + off,     8);
			memcpy(&nl, body + off + 8, 2);
			if (off + 10 + nl > blen) { rc = EIO; break; }
			rc = cb(ctx, ch,
			    (const char *)(body + off + 10), nl);
			if (rc != 0) break;
			off += 10 + nl;
		}
	} else if (k == TESSERA_MFT_DIRECTORY_2L) {
		const uint32_t nbk = tessera_manifest_parser_count(p);
		for (uint32_t bi = 0; bi < nbk && rc == 0; bi++) {
			tessera_dir_bucket_record_t br;
			if (tessera_manifest_dir_bucket_at(p, bi, &br)
			    != TESSERA_OK) { rc = EIO; break; }
			uint8_t *bbuf = NULL;
			uint32_t blen2 = 0;
			if (tessera_fs_fetch_blob(tmp_,
			    br.bucket_manifest_hash, &bbuf, &blen2) != 0) {
				rc = EIO; break;
			}
			if (blen2 < 32) {
				free(bbuf, M_TESSERA);
				rc = EIO; break;
			}
			const uint8_t *body = bbuf + 32;
			const size_t   blen = blen2 - 32;
			for (size_t off = 0; off + 10 <= blen; ) {
				uint64_t ch;
				uint16_t nl;
				memcpy(&ch, body + off,     8);
				memcpy(&nl, body + off + 8, 2);
				if (off + 10 + nl > blen) {
					rc = EIO; break;
				}
				rc = cb(ctx, ch,
				    (const char *)(body + off + 10), nl);
				if (rc != 0) break;
				off += 10 + nl;
			}
			free(bbuf, M_TESSERA);
		}
	} else if (k == TESSERA_MFT_DIRECTORY_BTREE) {
		/* B-tree directory: defer to the dedicated walker (which
		 * handles inner / leaf nodes recursively). The blob we
		 * already have is the root; we just need to free it and
		 * have the recursive walker re-fetch — keeps the walker
		 * uniform across the recursion. */
		tessera_manifest_parser_free(p);
		free(blob, M_TESSERA);
		return (tessera_fs_dir_btree_walk(tmp_, dir_manifest_hash,
		    cb, ctx));
	} else {
		rc = ENOTDIR;
	}

	tessera_manifest_parser_free(p);
	free(blob, M_TESSERA);
	return (rc);
}

/* Auto-promoting publish: takes a fully-built flat DIRECTORY manifest.
 * If under the threshold, publishes as-is. Otherwise re-parses the
 * flat body, splits dirents into TESSERA_DIR_BUCKET_COUNT hash
 * buckets, publishes each bucket as a flat DIRECTORY, and emits a
 * DIRECTORY_2L outer manifest pointing at them. Returns the outer's
 * hash via out_hash. */
static int
tessera_fs_publish_directory(struct tessera_mount *tmp_,
                             uint32_t owner_inode_no,
                             const uint8_t *flat_mft, size_t flat_mlen,
                             tessera_hash_t out_hash)
{
	if (flat_mlen <= TESSERA_DIR_PROMOTE_THRESHOLD) {
		return (tessera_fs_publish_manifest_owned(tmp_, flat_mft,
		    flat_mlen, out_hash, owner_inode_no));
	}

	/* Promote. Walk the flat body once, compute each entry's hash
	 * and bucket index, append to per-bucket builders. */
	if (flat_mlen < 32) return (EIO);
	const uint8_t *body = flat_mft + 32;
	const size_t   blen = flat_mlen - 32;

	const uint32_t K = TESSERA_DIR_BUCKET_COUNT;
	tessera_manifest_builder_t **bucket_mb =
	    malloc(K * sizeof *bucket_mb, M_TESSERA, M_WAITOK | M_ZERO);
	uint64_t *bucket_first = malloc(K * sizeof *bucket_first,
	    M_TESSERA, M_WAITOK);
	int *bucket_first_set = malloc(K * sizeof *bucket_first_set,
	    M_TESSERA, M_WAITOK | M_ZERO);
	uint32_t *bucket_count = malloc(K * sizeof *bucket_count,
	    M_TESSERA, M_WAITOK | M_ZERO);

	for (uint32_t i = 0; i < K; i++) {
		bucket_mb[i] = tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
		if (bucket_mb[i] == NULL) {
			while (i-- > 0) tessera_manifest_free(bucket_mb[i]);
			free(bucket_mb,        M_TESSERA);
			free(bucket_first,     M_TESSERA);
			free(bucket_first_set, M_TESSERA);
			free(bucket_count,     M_TESSERA);
			return (ENOMEM);
		}
	}

	for (size_t off = 0; off + 10 <= blen; ) {
		uint64_t ch;
		uint16_t nl;
		memcpy(&ch, body + off,     8);
		memcpy(&nl, body + off + 8, 2);
		if (off + 10 + nl > blen) {
			for (uint32_t i = 0; i < K; i++)
				tessera_manifest_free(bucket_mb[i]);
			free(bucket_mb,        M_TESSERA);
			free(bucket_first,     M_TESSERA);
			free(bucket_first_set, M_TESSERA);
			free(bucket_count,     M_TESSERA);
			return (EIO);
		}
		const char *nm = (const char *)(body + off + 10);
		uint64_t h = tessera_dir_name_hash(nm, nl);
		/* Bucket selection MUST be monotonic on h: tessera_fs_dir_2l_lookup
		 * binary-searches the outer manifest by first_name_hash, which
		 * only works if hashes partition the bucket space in order. The
		 * old (h >> 32) %% K mapping scrambled hashes across buckets
		 * non-monotonically — lookups for entries that weren't a
		 * bucket's smallest-hashed entry would land in the wrong bucket
		 * and return ENOENT (bug #3). With K = 16 = 2^4, the top 4 bits
		 * of h give a uniform monotonic placement. */
		uint32_t bi = (uint32_t)(h >> 56);
		(void)K; /* still used for sizing arrays / iteration below */
		if (tessera_manifest_add_dirent(bucket_mb[bi], ch, nm, nl)
		    != TESSERA_OK) {
			for (uint32_t i = 0; i < K; i++)
				tessera_manifest_free(bucket_mb[i]);
			free(bucket_mb,        M_TESSERA);
			free(bucket_first,     M_TESSERA);
			free(bucket_first_set, M_TESSERA);
			free(bucket_count,     M_TESSERA);
			return (ENOMEM);
		}
		if (!bucket_first_set[bi] || h < bucket_first[bi]) {
			bucket_first[bi]     = h;
			bucket_first_set[bi] = 1;
		}
		bucket_count[bi]++;
		off += 10 + nl;
	}

	/* Publish each non-empty bucket; collect (first_hash, hash) pairs. */
	tessera_manifest_builder_t *outer =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY_2L);
	if (outer == NULL) {
		for (uint32_t i = 0; i < K; i++)
			tessera_manifest_free(bucket_mb[i]);
		free(bucket_mb,        M_TESSERA);
		free(bucket_first,     M_TESSERA);
		free(bucket_first_set, M_TESSERA);
		free(bucket_count,     M_TESSERA);
		return (ENOMEM);
	}

	/* tessera_fs_dir_2l_lookup binary-searches the outer manifest's
	 * bucket records by first_name_hash, so they MUST be emitted in
	 * ascending first_name_hash order. Bucket index (h>>32 % K)
	 * doesn't correlate with first_name_hash, so build a sorted
	 * traversal order over non-empty buckets. K=16 → insertion sort
	 * is fine. (Bug #3: prior code emitted in i-order; binary search
	 * picked the wrong bucket → lookup ENOENT for entries readdir
	 * could see.) */
	/* Heap-allocate (K=256 → 1 KiB on stack; the surrounding helper
	 * is already deep into kbio + manifest, keep frame small). */
	uint32_t *order = malloc(K * sizeof *order, M_TESSERA, M_WAITOK);
	uint32_t n_order = 0;
	for (uint32_t i = 0; i < K; i++)
		if (bucket_count[i] > 0) order[n_order++] = i;
	for (uint32_t a = 1; a < n_order; a++) {
		uint32_t cur = order[a];
		uint64_t curh = bucket_first[cur];
		int32_t b = (int32_t)a - 1;
		while (b >= 0 && bucket_first[order[b]] > curh) {
			order[b + 1] = order[b];
			b--;
		}
		order[b + 1] = cur;
	}

	int err = 0;
	for (uint32_t k = 0; k < n_order; k++) {
		uint32_t i = order[k];
		size_t bmlen = 0;
		tessera_hash_t bmhash;
		(void)tessera_manifest_finalize(bucket_mb[i], NULL, 0,
		    &bmlen, bmhash);
		uint8_t *bbuf = malloc(bmlen, M_TESSERA, M_WAITOK);
		if (tessera_manifest_finalize(bucket_mb[i], bbuf, bmlen,
		    &bmlen, bmhash) != TESSERA_OK) {
			free(bbuf, M_TESSERA);
			err = EIO; break;
		}
		tessera_hash_t bpub;
		if (tessera_fs_publish_manifest(tmp_, bbuf, bmlen, bpub)
		    != 0) {
			free(bbuf, M_TESSERA);
			err = EIO; break;
		}
		free(bbuf, M_TESSERA);
		if (tessera_manifest_add_dir_bucket(outer,
		    bucket_first[i], bpub) != TESSERA_OK) {
			err = ENOMEM; break;
		}
	}
	/* Empty buckets aren't traversed in the loop above; free them now.
	 * NULL the slot so the catch-all cleanup below doesn't double-free. */
	for (uint32_t i = 0; i < K; i++) {
		if (bucket_count[i] == 0) {
			tessera_manifest_free(bucket_mb[i]);
			bucket_mb[i] = NULL;
		}
	}
	for (uint32_t i = 0; i < K; i++) {
		if (bucket_mb[i] != NULL) tessera_manifest_free(bucket_mb[i]);
	}
	free(bucket_mb,        M_TESSERA);
	free(bucket_first,     M_TESSERA);
	free(bucket_first_set, M_TESSERA);
	free(bucket_count,     M_TESSERA);
	free(order,            M_TESSERA);
	if (err != 0) {
		tessera_manifest_free(outer);
		return (err);
	}

	size_t omlen = 0;
	tessera_hash_t omhash;
	(void)tessera_manifest_finalize(outer, NULL, 0, &omlen, omhash);
	uint8_t *obuf = malloc(omlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(outer, obuf, omlen, &omlen, omhash)
	    != TESSERA_OK) {
		tessera_manifest_free(outer);
		free(obuf, M_TESSERA);
		return (EIO);
	}
	tessera_manifest_free(outer);
	/* Outer DIRECTORY_2L is owned by the parent inode; supersession
	 * fires when the next dir mutation rewrites it. Inner buckets
	 * stay untagged (multiple buckets per dir). */
	int rc = tessera_fs_publish_manifest_owned(tmp_, obuf, omlen,
	    out_hash, owner_inode_no);
	free(obuf, M_TESSERA);
	return (rc);
}

/* ──────────────────────────────────────────────────────────────────
 * v2.5 directory B-tree (TESSERA_MFT_DIRECTORY_BTREE)
 *
 * Fully content-addressed: each tree node is a manifest blob, the
 * dir's manifest_hash points at the root node. Mutation = COW path
 * of O(log_F N) nodes; lookup = O(log_F N) descent. Replaces
 * DIRECTORY_2L's O(N/K) per-op cost with proper logarithmic scaling
 * while preserving the design invariant that equal-content dirs
 * produce equal hashes (same as inode_tree / pack_registry).
 *
 * Node body layout: [u8 leaf_flag][u8 reserved×3][u32 reserved] then
 * a stream of records.
 *   Inner record (40 B): u64 max_name_hash, tessera_hash_t child_hash
 *   Leaf  record (var):  u64 name_hash, u64 inode_no,
 *                        u16 name_len, name bytes
 *
 * Leaves split when count > TESSERA_DIR_BTREE_FANOUT_LEAF;
 * inners split when count > TESSERA_DIR_BTREE_FANOUT_INNER. Removes
 * leave underfilled nodes (no merge in v1 — they shrink naturally
 * as the workload mutates). Records within a node are kept sorted
 * by name_hash so binary search drives lookup.
 * ────────────────────────────────────────────────────────────────── */

/* Result of inserting/removing into a subtree. KEEP = single new
 * node hash; SPLIT = two siblings + their key ranges. */
struct tessera_btree_op_result {
	int       kind;             /* 0 = KEEP, 1 = SPLIT, 2 = DROPPED */
	uint64_t  left_max_hash;
	tessera_hash_t left_hash;
	uint64_t  right_max_hash;
	tessera_hash_t right_hash;
};
#define TESSERA_BTREE_KEEP     0
#define TESSERA_BTREE_SPLIT    1
#define TESSERA_BTREE_DROPPED  2

/* Decode a node blob into (leaf_flag, count, body, body_len). The
 * caller owns the blob memory. */
static int
tessera_fs_dir_btree_decode(uint8_t *blob, uint32_t blob_len,
    int *out_leaf, const uint8_t **out_body, size_t *out_body_len,
    uint32_t *out_count)
{
	if (blob_len < 32 + 8) return (EIO);
	tessera_manifest_parser_t *p = tessera_manifest_parse(blob, blob_len);
	if (p == NULL) return (EIO);
	if (tessera_manifest_parser_kind(p) != TESSERA_MFT_DIRECTORY_BTREE) {
		tessera_manifest_parser_free(p);
		return (EIO);
	}
	*out_count = tessera_manifest_parser_count(p);
	tessera_manifest_parser_free(p);
	const uint8_t *body = blob + 32;
	*out_leaf = body[0] ? 1 : 0;
	*out_body = body + 8;
	*out_body_len = blob_len - 32 - 8;
	return (0);
}

/* Walk a leaf node: callback per (inode_no, name, name_len). Returns
 * 0 on full success, or the callback's nonzero return to abort. */
static int
tessera_fs_dir_btree_walk_leaf(const uint8_t *body, size_t body_len,
    uint32_t count, tessera_dirent_cb_t cb, void *ctx)
{
	size_t off = 0;
	for (uint32_t i = 0; i < count; i++) {
		if (off + 8 + 8 + 2 > body_len) return (EIO);
		uint64_t inode_no;
		uint16_t name_len;
		memcpy(&inode_no, body + off + 8, 8);
		memcpy(&name_len, body + off + 16, 2);
		if (off + 18 + name_len > body_len) return (EIO);
		const char *name = (const char *)(body + off + 18);
		int rc = cb(ctx, inode_no, name, name_len);
		if (rc != 0) return (rc);
		off += 18 + name_len;
	}
	return (0);
}

/* Recursive walker. Reads node, dispatches by kind. */
static int
tessera_fs_dir_btree_walk(struct tessera_mount *tmp_,
    const tessera_hash_t node_hash, tessera_dirent_cb_t cb, void *ctx)
{
	uint8_t  *blob = NULL;
	uint32_t  blob_len = 0;
	if (tessera_fs_fetch_blob(tmp_, node_hash, &blob, &blob_len) != 0)
		return (EIO);
	int leaf_flag;
	const uint8_t *body;
	size_t body_len;
	uint32_t count;
	int rc = tessera_fs_dir_btree_decode(blob, blob_len, &leaf_flag,
	    &body, &body_len, &count);
	if (rc != 0) { free(blob, M_TESSERA); return (rc); }
	if (leaf_flag) {
		rc = tessera_fs_dir_btree_walk_leaf(body, body_len, count,
		    cb, ctx);
	} else {
		size_t off = 0;
		for (uint32_t i = 0; i < count && rc == 0; i++) {
			if (off + 8 + TESSERA_HASH_SIZE > body_len) {
				rc = EIO; break;
			}
			tessera_hash_t child;
			memcpy(child, body + off + 8, TESSERA_HASH_SIZE);
			rc = tessera_fs_dir_btree_walk(tmp_, child, cb, ctx);
			off += 8 + TESSERA_HASH_SIZE;
		}
	}
	free(blob, M_TESSERA);
	return (rc);
}

/* Lookup `name` in a B-tree directory. Returns 0 + *out_inode on hit,
 * ENOENT if not found. */
static int
tessera_fs_dir_btree_lookup(struct tessera_mount *tmp_,
    const tessera_hash_t node_hash, const char *name, uint16_t namelen,
    uint64_t *out_inode)
{
	const uint64_t key = tessera_dir_name_hash(name, namelen);
	tessera_hash_t cur;
	memcpy(cur, node_hash, TESSERA_HASH_SIZE);

	for (int depth = 0; depth < 32; depth++) {
		uint8_t  *blob = NULL;
		uint32_t  blob_len = 0;
		if (tessera_fs_fetch_blob(tmp_, cur, &blob, &blob_len) != 0)
			return (EIO);
		int leaf;
		const uint8_t *body;
		size_t body_len;
		uint32_t count;
		int rc = tessera_fs_dir_btree_decode(blob, blob_len, &leaf,
		    &body, &body_len, &count);
		if (rc != 0) { free(blob, M_TESSERA); return (rc); }

		if (leaf) {
			size_t off = 0;
			int found_enoent = 1;
			for (uint32_t i = 0; i < count; i++) {
				if (off + 18 > body_len) {
					free(blob, M_TESSERA);
					return (EIO);
				}
				uint64_t h;
				uint64_t ino;
				uint16_t nl;
				memcpy(&h, body + off, 8);
				memcpy(&ino, body + off + 8, 8);
				memcpy(&nl, body + off + 16, 2);
				if (off + 18 + nl > body_len) {
					free(blob, M_TESSERA);
					return (EIO);
				}
				if (h == key && nl == namelen &&
				    memcmp(body + off + 18, name, namelen) == 0) {
					*out_inode = ino;
					free(blob, M_TESSERA);
					return (0);
				}
				if (h > key) { found_enoent = 1; break; }
				off += 18 + nl;
			}
			free(blob, M_TESSERA);
			(void)found_enoent;
			return (ENOENT);
		}

		/* Inner: find first child with max_name_hash >= key. */
		size_t off = 0;
		int picked = 0;
		for (uint32_t i = 0; i < count; i++) {
			if (off + 8 + TESSERA_HASH_SIZE > body_len) {
				free(blob, M_TESSERA);
				return (EIO);
			}
			uint64_t mh;
			memcpy(&mh, body + off, 8);
			if (mh >= key) {
				memcpy(cur, body + off + 8, TESSERA_HASH_SIZE);
				picked = 1;
				break;
			}
			off += 8 + TESSERA_HASH_SIZE;
		}
		free(blob, M_TESSERA);
		if (!picked) return (ENOENT);
	}
	return (EIO);  /* depth overflow */
}

/* Build a leaf node from a sorted in-memory list of records. */
static int
tessera_fs_dir_btree_publish_leaf(struct tessera_mount *tmp_,
    const uint64_t *hashes, const uint64_t *inos,
    const char *const *names, const uint16_t *nlens,
    uint32_t count, tessera_hash_t out_hash)
{
	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY_BTREE);
	if (mb == NULL) return (ENOMEM);
	if (tessera_manifest_dir_btree_set_leaf(mb, 1) != TESSERA_OK) {
		tessera_manifest_free(mb);
		return (ENOMEM);
	}
	for (uint32_t i = 0; i < count; i++) {
		if (tessera_manifest_dir_btree_add_leaf(mb,
		    hashes[i], inos[i], names[i], nlens[i]) != TESSERA_OK) {
			tessera_manifest_free(mb);
			return (ENOMEM);
		}
	}
	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	uint8_t *buf = malloc(mlen, M_TESSERA, M_WAITOK);
	int rc = tessera_manifest_finalize(mb, buf, mlen, &mlen, mhash);
	tessera_manifest_free(mb);
	if (rc != TESSERA_OK) { free(buf, M_TESSERA); return (EIO); }
	/* known_new: leaf bytes were just synthesised from the just-
	 * mutated in-memory record list, so by construction unique. */
	rc = tessera_fs_publish_manifest_owned_known_new(tmp_, buf, mlen,
	    out_hash, /*owner_inode_no=*/0);
	free(buf, M_TESSERA);
	return (rc == 0 ? 0 : EIO);
}

/* Build an inner node from a list of (max_hash, child_hash) pairs. */
static int
tessera_fs_dir_btree_publish_inner(struct tessera_mount *tmp_,
    const uint64_t *max_hashes, const tessera_hash_t *child_hashes,
    uint32_t count, tessera_hash_t out_hash)
{
	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY_BTREE);
	if (mb == NULL) return (ENOMEM);
	if (tessera_manifest_dir_btree_set_leaf(mb, 0) != TESSERA_OK) {
		tessera_manifest_free(mb);
		return (ENOMEM);
	}
	for (uint32_t i = 0; i < count; i++) {
		if (tessera_manifest_dir_btree_add_inner(mb,
		    max_hashes[i], child_hashes[i]) != TESSERA_OK) {
			tessera_manifest_free(mb);
			return (ENOMEM);
		}
	}
	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	uint8_t *buf = malloc(mlen, M_TESSERA, M_WAITOK);
	int rc = tessera_manifest_finalize(mb, buf, mlen, &mlen, mhash);
	tessera_manifest_free(mb);
	if (rc != TESSERA_OK) { free(buf, M_TESSERA); return (EIO); }
	rc = tessera_fs_publish_manifest_owned_known_new(tmp_, buf, mlen,
	    out_hash, /*owner_inode_no=*/0);
	free(buf, M_TESSERA);
	return (rc == 0 ? 0 : EIO);
}

/* Forward decl — recursive insert. */
static int
tessera_fs_dir_btree_insert_at(struct tessera_mount *tmp_,
    const tessera_hash_t node_hash, uint64_t key, uint64_t inode_no,
    const char *name, uint16_t namelen,
    struct tessera_btree_op_result *out);

/* Insert into a leaf node. Builds new leaf (with the inserted entry)
 * and either KEEPs (if size <= FANOUT) or SPLITs into two halves. */
static int
tessera_fs_dir_btree_leaf_insert(struct tessera_mount *tmp_,
    const uint8_t *body, size_t body_len, uint32_t count,
    uint64_t key, uint64_t inode_no, const char *name, uint16_t namelen,
    struct tessera_btree_op_result *out)
{
	/* Decode existing entries into arrays. */
	uint32_t cap = count + 1;
	uint64_t *hashes  = malloc(cap * sizeof *hashes,  M_TESSERA, M_WAITOK);
	uint64_t *inos    = malloc(cap * sizeof *inos,    M_TESSERA, M_WAITOK);
	const char **nptrs = malloc(cap * sizeof *nptrs,  M_TESSERA, M_WAITOK);
	uint16_t  *nlens  = malloc(cap * sizeof *nlens,   M_TESSERA, M_WAITOK);
	uint8_t  **owned  = malloc(cap * sizeof *owned,   M_TESSERA, M_WAITOK | M_ZERO);

	size_t off = 0;
	uint32_t out_count = 0;
	int inserted = 0;
	for (uint32_t i = 0; i < count; i++) {
		uint64_t h;
		uint64_t ino;
		uint16_t nl;
		memcpy(&h, body + off, 8);
		memcpy(&ino, body + off + 8, 8);
		memcpy(&nl, body + off + 16, 2);
		const char *nm = (const char *)(body + off + 18);
		if (!inserted && (h > key ||
		    (h == key && (nl > namelen ||
		     (nl == namelen && memcmp(nm, name, nl) > 0))))) {
			hashes[out_count] = key;
			inos[out_count]   = inode_no;
			nptrs[out_count]  = name;
			nlens[out_count]  = namelen;
			owned[out_count]  = NULL;
			out_count++;
			inserted = 1;
		}
		if (h == key && nl == namelen &&
		    memcmp(nm, name, namelen) == 0) {
			/* Duplicate. Caller's responsibility — return
			 * EEXIST. */
			free(hashes, M_TESSERA); free(inos, M_TESSERA);
			free(nptrs,  M_TESSERA); free(nlens, M_TESSERA);
			for (uint32_t j = 0; j < out_count; j++)
				if (owned[j]) free(owned[j], M_TESSERA);
			free(owned, M_TESSERA);
			return (EEXIST);
		}
		hashes[out_count] = h;
		inos[out_count]   = ino;
		/* Names point into `body` which lives until our caller
		 * frees the blob. We stash refs only; the publish path
		 * memcpys before we return. */
		nptrs[out_count]  = nm;
		nlens[out_count]  = nl;
		owned[out_count]  = NULL;
		out_count++;
		off += 18 + nl;
	}
	if (!inserted) {
		hashes[out_count] = key;
		inos[out_count]   = inode_no;
		nptrs[out_count]  = name;
		nlens[out_count]  = namelen;
		owned[out_count]  = NULL;
		out_count++;
	}

	int rc = 0;
	if (out_count <= TESSERA_DIR_BTREE_FANOUT_LEAF) {
		out->kind = TESSERA_BTREE_KEEP;
		rc = tessera_fs_dir_btree_publish_leaf(tmp_, hashes, inos,
		    nptrs, nlens, out_count, out->left_hash);
		out->left_max_hash = hashes[out_count - 1];
	} else {
		uint32_t half = out_count / 2;
		rc = tessera_fs_dir_btree_publish_leaf(tmp_, hashes, inos,
		    nptrs, nlens, half, out->left_hash);
		if (rc == 0) {
			out->left_max_hash = hashes[half - 1];
			rc = tessera_fs_dir_btree_publish_leaf(tmp_,
			    hashes + half, inos + half, nptrs + half,
			    nlens + half, out_count - half, out->right_hash);
			out->right_max_hash = hashes[out_count - 1];
			out->kind = TESSERA_BTREE_SPLIT;
		}
	}

	free(hashes, M_TESSERA); free(inos, M_TESSERA);
	free(nptrs,  M_TESSERA); free(nlens, M_TESSERA);
	for (uint32_t j = 0; j < out_count; j++)
		if (owned[j]) free(owned[j], M_TESSERA);
	free(owned, M_TESSERA);
	return (rc);
}

static int
tessera_fs_dir_btree_inner_insert_after_split(struct tessera_mount *tmp_,
    const uint8_t *body, size_t body_len, uint32_t count,
    uint32_t target_idx, uint64_t left_max, const tessera_hash_t left_hash,
    uint64_t right_max, const tessera_hash_t right_hash,
    int split_happened, struct tessera_btree_op_result *out)
{
	/* Build a new inner node. If split_happened, replace the
	 * target_idx entry with TWO entries; else replace it with one
	 * (just the new child hash since the recursion KEPT). */
	uint32_t new_count = count + (split_happened ? 1 : 0);
	uint64_t *maxes = malloc(new_count * sizeof *maxes, M_TESSERA, M_WAITOK);
	tessera_hash_t *children = malloc(new_count * sizeof *children,
	    M_TESSERA, M_WAITOK);

	size_t off = 0;
	uint32_t k = 0;
	for (uint32_t i = 0; i < count; i++) {
		uint64_t mh;
		memcpy(&mh, body + off, 8);
		const uint8_t *ch = body + off + 8;
		if (i == target_idx) {
			maxes[k] = left_max;
			memcpy(children[k], left_hash, TESSERA_HASH_SIZE);
			k++;
			if (split_happened) {
				maxes[k] = right_max;
				memcpy(children[k], right_hash,
				    TESSERA_HASH_SIZE);
				k++;
			}
		} else {
			maxes[k] = mh;
			memcpy(children[k], ch, TESSERA_HASH_SIZE);
			k++;
		}
		off += 8 + TESSERA_HASH_SIZE;
	}

	int rc = 0;
	if (k <= TESSERA_DIR_BTREE_FANOUT_INNER) {
		out->kind = TESSERA_BTREE_KEEP;
		rc = tessera_fs_dir_btree_publish_inner(tmp_, maxes, children,
		    k, out->left_hash);
		out->left_max_hash = maxes[k - 1];
	} else {
		uint32_t half = k / 2;
		rc = tessera_fs_dir_btree_publish_inner(tmp_, maxes, children,
		    half, out->left_hash);
		if (rc == 0) {
			out->left_max_hash = maxes[half - 1];
			rc = tessera_fs_dir_btree_publish_inner(tmp_,
			    maxes + half, children + half, k - half,
			    out->right_hash);
			out->right_max_hash = maxes[k - 1];
			out->kind = TESSERA_BTREE_SPLIT;
		}
	}

	free(maxes, M_TESSERA);
	free(children, M_TESSERA);
	return (rc);
}

static int
tessera_fs_dir_btree_insert_at(struct tessera_mount *tmp_,
    const tessera_hash_t node_hash, uint64_t key, uint64_t inode_no,
    const char *name, uint16_t namelen,
    struct tessera_btree_op_result *out)
{
	uint8_t  *blob = NULL;
	uint32_t  blob_len = 0;
	if (tessera_fs_fetch_blob(tmp_, node_hash, &blob, &blob_len) != 0)
		return (EIO);
	int leaf;
	const uint8_t *body;
	size_t body_len;
	uint32_t count;
	int rc = tessera_fs_dir_btree_decode(blob, blob_len, &leaf,
	    &body, &body_len, &count);
	if (rc != 0) { free(blob, M_TESSERA); return (rc); }

	if (leaf) {
		rc = tessera_fs_dir_btree_leaf_insert(tmp_, body, body_len,
		    count, key, inode_no, name, namelen, out);
		free(blob, M_TESSERA);
		return (rc);
	}

	/* Inner: find first child where max_name_hash >= key (or last
	 * child if key beyond all maxes — extend rightmost). */
	size_t off = 0;
	uint32_t target = 0;
	int found = 0;
	for (uint32_t i = 0; i < count; i++) {
		uint64_t mh;
		memcpy(&mh, body + off, 8);
		if (mh >= key) { target = i; found = 1; break; }
		off += 8 + TESSERA_HASH_SIZE;
	}
	if (!found) {
		target = count - 1;
		off = (size_t)target * (8 + TESSERA_HASH_SIZE);
	}
	tessera_hash_t child;
	memcpy(child, body + off + 8, TESSERA_HASH_SIZE);

	struct tessera_btree_op_result child_res;
	memset(&child_res, 0, sizeof child_res);
	rc = tessera_fs_dir_btree_insert_at(tmp_, child, key, inode_no,
	    name, namelen, &child_res);
	if (rc != 0) {
		free(blob, M_TESSERA);
		return (rc);
	}

	int split = (child_res.kind == TESSERA_BTREE_SPLIT);
	rc = tessera_fs_dir_btree_inner_insert_after_split(tmp_, body,
	    body_len, count, target,
	    child_res.left_max_hash, child_res.left_hash,
	    split ? child_res.right_max_hash : 0,
	    split ? child_res.right_hash : (const uint8_t *)child_res.left_hash,
	    split, out);
	free(blob, M_TESSERA);
	return (rc);
}

/* Top-level insert: handles the empty-tree, and root-split cases. */
static int
tessera_fs_dir_btree_insert(struct tessera_mount *tmp_,
    const tessera_hash_t root_hash, int root_is_empty,
    const char *name, uint16_t namelen, uint64_t inode_no,
    tessera_hash_t out_new_root)
{
	uint64_t key = tessera_dir_name_hash(name, namelen);
	struct tessera_btree_op_result res;
	memset(&res, 0, sizeof res);

	if (root_is_empty) {
		/* New tree — single leaf with one entry. */
		const char *nptr = name;
		uint16_t nl = namelen;
		uint64_t k = key;
		uint64_t v = inode_no;
		int rc = tessera_fs_dir_btree_publish_leaf(tmp_, &k, &v,
		    &nptr, &nl, 1, out_new_root);
		return (rc);
	}

	int rc = tessera_fs_dir_btree_insert_at(tmp_, root_hash, key,
	    inode_no, name, namelen, &res);
	if (rc != 0) return (rc);

	if (res.kind == TESSERA_BTREE_KEEP) {
		memcpy(out_new_root, res.left_hash, TESSERA_HASH_SIZE);
		return (0);
	}
	/* SPLIT: build a new root. */
	uint64_t maxes[2] = { res.left_max_hash, res.right_max_hash };
	tessera_hash_t children[2];
	memcpy(children[0], res.left_hash,  TESSERA_HASH_SIZE);
	memcpy(children[1], res.right_hash, TESSERA_HASH_SIZE);
	return (tessera_fs_dir_btree_publish_inner(tmp_, maxes, children, 2,
	    out_new_root));
}

/* Forward decl — recursive remove. */
static int
tessera_fs_dir_btree_remove_at(struct tessera_mount *tmp_,
    const tessera_hash_t node_hash, uint64_t key,
    const char *name, uint16_t namelen, uint64_t verify_inode,
    struct tessera_btree_op_result *out);

static int
tessera_fs_dir_btree_remove_at(struct tessera_mount *tmp_,
    const tessera_hash_t node_hash, uint64_t key,
    const char *name, uint16_t namelen, uint64_t verify_inode,
    struct tessera_btree_op_result *out)
{
	uint8_t  *blob = NULL;
	uint32_t  blob_len = 0;
	if (tessera_fs_fetch_blob(tmp_, node_hash, &blob, &blob_len) != 0)
		return (EIO);
	int leaf;
	const uint8_t *body;
	size_t body_len;
	uint32_t count;
	int rc = tessera_fs_dir_btree_decode(blob, blob_len, &leaf,
	    &body, &body_len, &count);
	if (rc != 0) { free(blob, M_TESSERA); return (rc); }

	if (leaf) {
		/* Build new leaf, skipping the matched entry. */
		uint64_t *hashes  = malloc(count * sizeof *hashes,
		    M_TESSERA, M_WAITOK);
		uint64_t *inos    = malloc(count * sizeof *inos,
		    M_TESSERA, M_WAITOK);
		const char **nptrs = malloc(count * sizeof *nptrs,
		    M_TESSERA, M_WAITOK);
		uint16_t  *nlens  = malloc(count * sizeof *nlens,
		    M_TESSERA, M_WAITOK);
		size_t off = 0;
		uint32_t k = 0;
		int matched = 0;
		for (uint32_t i = 0; i < count; i++) {
			uint64_t h;
			uint64_t ino;
			uint16_t nl;
			memcpy(&h, body + off, 8);
			memcpy(&ino, body + off + 8, 8);
			memcpy(&nl, body + off + 16, 2);
			const char *nm = (const char *)(body + off + 18);
			if (h == key && nl == namelen &&
			    memcmp(nm, name, namelen) == 0) {
				if (verify_inode != 0 &&
				    ino != verify_inode) {
					free(hashes, M_TESSERA);
					free(inos,   M_TESSERA);
					free(nptrs,  M_TESSERA);
					free(nlens,  M_TESSERA);
					free(blob,   M_TESSERA);
					return (EIO);
				}
				matched = 1;
			} else {
				hashes[k] = h; inos[k] = ino;
				nptrs[k]  = nm; nlens[k] = nl;
				k++;
			}
			off += 18 + nl;
		}
		if (!matched) {
			free(hashes, M_TESSERA); free(inos, M_TESSERA);
			free(nptrs,  M_TESSERA); free(nlens, M_TESSERA);
			free(blob, M_TESSERA);
			return (ENOENT);
		}
		if (k == 0) {
			out->kind = TESSERA_BTREE_DROPPED;
		} else {
			rc = tessera_fs_dir_btree_publish_leaf(tmp_,
			    hashes, inos, nptrs, nlens, k, out->left_hash);
			out->kind = TESSERA_BTREE_KEEP;
			out->left_max_hash = hashes[k - 1];
		}
		free(hashes, M_TESSERA); free(inos, M_TESSERA);
		free(nptrs,  M_TESSERA); free(nlens, M_TESSERA);
		free(blob, M_TESSERA);
		return (rc);
	}

	/* Inner: find target child, recurse. */
	size_t off = 0;
	uint32_t target = 0;
	int found = 0;
	for (uint32_t i = 0; i < count; i++) {
		uint64_t mh;
		memcpy(&mh, body + off, 8);
		if (mh >= key) { target = i; found = 1; break; }
		off += 8 + TESSERA_HASH_SIZE;
	}
	if (!found) {
		free(blob, M_TESSERA);
		return (ENOENT);
	}
	tessera_hash_t child;
	memcpy(child, body + off + 8, TESSERA_HASH_SIZE);

	struct tessera_btree_op_result child_res;
	memset(&child_res, 0, sizeof child_res);
	rc = tessera_fs_dir_btree_remove_at(tmp_, child, key, name,
	    namelen, verify_inode, &child_res);
	if (rc != 0) { free(blob, M_TESSERA); return (rc); }

	/* Rebuild this inner: replace target entry with new child (or
	 * drop it if child was DROPPED). */
	uint32_t new_count = count - (child_res.kind == TESSERA_BTREE_DROPPED ?
	    1 : 0);
	if (new_count == 0) {
		out->kind = TESSERA_BTREE_DROPPED;
		free(blob, M_TESSERA);
		return (0);
	}
	uint64_t *maxes = malloc(new_count * sizeof *maxes, M_TESSERA, M_WAITOK);
	tessera_hash_t *children = malloc(new_count * sizeof *children,
	    M_TESSERA, M_WAITOK);
	off = 0;
	uint32_t k = 0;
	for (uint32_t i = 0; i < count; i++) {
		uint64_t mh;
		memcpy(&mh, body + off, 8);
		const uint8_t *ch = body + off + 8;
		if (i == target) {
			if (child_res.kind != TESSERA_BTREE_DROPPED) {
				maxes[k] = child_res.left_max_hash;
				memcpy(children[k], child_res.left_hash,
				    TESSERA_HASH_SIZE);
				k++;
			}
			/* DROPPED: skip. */
		} else {
			maxes[k] = mh;
			memcpy(children[k], ch, TESSERA_HASH_SIZE);
			k++;
		}
		off += 8 + TESSERA_HASH_SIZE;
	}
	rc = tessera_fs_dir_btree_publish_inner(tmp_, maxes, children, k,
	    out->left_hash);
	out->kind = TESSERA_BTREE_KEEP;
	out->left_max_hash = (k > 0) ? maxes[k - 1] : 0;
	free(maxes, M_TESSERA);
	free(children, M_TESSERA);
	free(blob, M_TESSERA);
	return (rc);
}

/* Top-level remove: collapses single-child inner roots back to their
 * lone child so tree height shrinks naturally. */
static int
tessera_fs_dir_btree_remove(struct tessera_mount *tmp_,
    const tessera_hash_t root_hash,
    const char *name, uint16_t namelen, uint64_t verify_inode,
    int *out_dropped, tessera_hash_t out_new_root)
{
	uint64_t key = tessera_dir_name_hash(name, namelen);
	struct tessera_btree_op_result res;
	memset(&res, 0, sizeof res);
	int rc = tessera_fs_dir_btree_remove_at(tmp_, root_hash, key,
	    name, namelen, verify_inode, &res);
	if (rc != 0) return (rc);
	if (res.kind == TESSERA_BTREE_DROPPED) {
		*out_dropped = 1;
		return (0);
	}
	*out_dropped = 0;
	memcpy(out_new_root, res.left_hash, TESSERA_HASH_SIZE);
	return (0);
}

/* Migrate a flat DIRECTORY or DIRECTORY_2L parent to BTREE.
 * Walks all entries, sorts by name_hash, then bulk-builds the
 * tree bottom-up. Returns the new root hash. */
struct migrate_collect_ctx {
	uint32_t  cap;
	uint32_t  count;
	uint64_t *hashes;
	uint64_t *inos;
	char    **names;
	uint16_t *nlens;
	int       err;
};

static int
migrate_collect_cb(void *vctx, uint64_t inode_no, const char *name,
                   uint16_t name_len)
{
	struct migrate_collect_ctx *c = vctx;
	if (c->count == c->cap) {
		uint32_t ncap = c->cap == 0 ? 64 : c->cap * 2;
		uint64_t *nh = malloc(ncap * sizeof *nh, M_TESSERA, M_WAITOK);
		uint64_t *ni = malloc(ncap * sizeof *ni, M_TESSERA, M_WAITOK);
		char    **nn = malloc(ncap * sizeof *nn, M_TESSERA, M_WAITOK);
		uint16_t *nl = malloc(ncap * sizeof *nl, M_TESSERA, M_WAITOK);
		if (c->cap > 0) {
			memcpy(nh, c->hashes, c->cap * sizeof *nh);
			memcpy(ni, c->inos,   c->cap * sizeof *ni);
			memcpy(nn, c->names,  c->cap * sizeof *nn);
			memcpy(nl, c->nlens,  c->cap * sizeof *nl);
			free(c->hashes, M_TESSERA); free(c->inos, M_TESSERA);
			free(c->names,  M_TESSERA); free(c->nlens, M_TESSERA);
		}
		c->hashes = nh; c->inos = ni; c->names = nn; c->nlens = nl;
		c->cap = ncap;
	}
	uint32_t i = c->count++;
	c->hashes[i] = tessera_dir_name_hash(name, name_len);
	c->inos[i]   = inode_no;
	char *copy = malloc(name_len, M_TESSERA, M_WAITOK);
	memcpy(copy, name, name_len);
	c->names[i]  = copy;
	c->nlens[i]  = name_len;
	return (0);
}

static int
tessera_fs_dir_btree_migrate(struct tessera_mount *tmp_,
    const tessera_hash_t old_dir_hash, tessera_hash_t out_new_root,
    int *out_empty)
{
	struct migrate_collect_ctx c = { 0 };
	int rc = tessera_fs_dir_walk(tmp_, old_dir_hash, migrate_collect_cb,
	    &c);
	if (rc != 0 && rc != ENOTDIR) {
		for (uint32_t i = 0; i < c.count; i++)
			free(c.names[i], M_TESSERA);
		free(c.hashes, M_TESSERA); free(c.inos, M_TESSERA);
		free(c.names,  M_TESSERA); free(c.nlens, M_TESSERA);
		return (rc);
	}
	if (c.count == 0) {
		*out_empty = 1;
		free(c.hashes, M_TESSERA); free(c.inos, M_TESSERA);
		free(c.names,  M_TESSERA); free(c.nlens, M_TESSERA);
		return (0);
	}
	*out_empty = 0;

	/* Insertion sort by name_hash (typical migration is small). */
	for (uint32_t i = 1; i < c.count; i++) {
		uint64_t h = c.hashes[i];
		uint64_t in = c.inos[i];
		char *nm = c.names[i];
		uint16_t nl = c.nlens[i];
		int32_t j = (int32_t)i - 1;
		while (j >= 0 && c.hashes[j] > h) {
			c.hashes[j+1] = c.hashes[j];
			c.inos[j+1]   = c.inos[j];
			c.names[j+1]  = c.names[j];
			c.nlens[j+1]  = c.nlens[j];
			j--;
		}
		c.hashes[j+1] = h; c.inos[j+1] = in;
		c.names[j+1] = nm; c.nlens[j+1] = nl;
	}

	/* Bulk-load: split into FANOUT_LEAF-sized leaves, then repeatedly
	 * build inner layers until one node remains. */
	uint32_t F = TESSERA_DIR_BTREE_FANOUT_LEAF;
	uint32_t leaf_count = (c.count + F - 1) / F;
	tessera_hash_t *layer = malloc(leaf_count * sizeof *layer,
	    M_TESSERA, M_WAITOK);
	uint64_t *layer_max = malloc(leaf_count * sizeof *layer_max,
	    M_TESSERA, M_WAITOK);

	for (uint32_t i = 0; i < leaf_count; i++) {
		uint32_t start = i * F;
		uint32_t take = c.count - start;
		if (take > F) take = F;
		const char *const *nptrs = (const char *const *)&c.names[start];
		rc = tessera_fs_dir_btree_publish_leaf(tmp_,
		    c.hashes + start, c.inos + start, nptrs, c.nlens + start,
		    take, layer[i]);
		if (rc != 0) goto out;
		layer_max[i] = c.hashes[start + take - 1];
	}

	uint32_t IF = TESSERA_DIR_BTREE_FANOUT_INNER;
	while (leaf_count > 1) {
		uint32_t up = (leaf_count + IF - 1) / IF;
		tessera_hash_t *next = malloc(up * sizeof *next,
		    M_TESSERA, M_WAITOK);
		uint64_t *next_max = malloc(up * sizeof *next_max,
		    M_TESSERA, M_WAITOK);
		for (uint32_t i = 0; i < up; i++) {
			uint32_t start = i * IF;
			uint32_t take = leaf_count - start;
			if (take > IF) take = IF;
			rc = tessera_fs_dir_btree_publish_inner(tmp_,
			    layer_max + start, layer + start, take, next[i]);
			if (rc != 0) {
				free(next, M_TESSERA);
				free(next_max, M_TESSERA);
				goto out;
			}
			next_max[i] = layer_max[start + take - 1];
		}
		free(layer, M_TESSERA);
		free(layer_max, M_TESSERA);
		layer = next;
		layer_max = next_max;
		leaf_count = up;
	}
	memcpy(out_new_root, layer[0], TESSERA_HASH_SIZE);

out:
	free(layer, M_TESSERA);
	free(layer_max, M_TESSERA);
	for (uint32_t i = 0; i < c.count; i++)
		free(c.names[i], M_TESSERA);
	free(c.hashes, M_TESSERA); free(c.inos, M_TESSERA);
	free(c.names,  M_TESSERA); free(c.nlens, M_TESSERA);
	return (rc);
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
tessera_chunk_size_for(struct tessera_mount *tmp_, uint64_t file_size)
{
	/* Per-mount override (v2 step-3c prereq). Validated power-of-2
	 * at mount time; we trust it here. */
	if (tmp_ != NULL && tmp_->chunk_size_override != 0)
		return (tmp_->chunk_size_override);

	if (file_size <  (64ULL * 1024ULL * 1024ULL))         /* < 64 MiB */
		return ( 64u * 1024u);
	if (file_size <  ( 4ULL * 1024ULL * 1024ULL * 1024ULL))/* < 4 GiB */
		return (  1u * 1024u * 1024u);
	return (  4u * 1024u * 1024u);                        /* ≥ 4 GiB */
}

/*
 * v2 step-3c: CHUNK_TREE write-side promotion.
 *
 * Once a file has more than TESSERA_CHUNK_TREE_FANOUT chunks at the
 * current chunk size, a flat CHUNK_LIST manifest becomes prohibitive:
 * a 1 GiB file at 4 KiB chunks would emit ~262144 chunk_records, a
 * ~12 MiB manifest body that's rewritten in full on every modifying
 * write.
 *
 * CHUNK_TREE partitions the chunks into K groups of at most FANOUT,
 * publishes each group as its own CHUNK_LIST sub-manifest pack, and
 * emits a single outer CHUNK_TREE manifest pointing at the K group
 * hashes. Read-side recursion is already in place (read_into_uio).
 *
 * Manifest cost amortizes to O(K + N/K) bytes per write — for FANOUT
 * = 256, that's ~50 KiB outer + ~50 KiB per dirty group instead of
 * ~12 MiB monolithic. Combined with per-group dedup against the old
 * tree (TODO follow-up), unchanged groups skip republish entirely.
 *
 * v1 scope: rebuild every group on every write (no inter-tree
 * dedup yet). Sub-CHUNK_LIST chunks use *global* logical offsets so
 * the existing read path doesn't need to know it's inside a subtree.
 */
#define TESSERA_CHUNK_TREE_FANOUT  256u

static int
tessera_fs_replace_content_chunk_tree(struct tessera_mount *tmp_,
    uint32_t inode_no, const uint8_t *new_bytes, size_t new_len,
    uint32_t cs)
{
	const uint32_t n_chunks = (uint32_t)((new_len + cs - 1) / cs);
	const uint32_t fanout   = TESSERA_CHUNK_TREE_FANOUT;
	const uint32_t n_groups = (n_chunks + fanout - 1) / fanout;

	tessera_manifest_builder_t *outer =
	    tessera_manifest_begin(TESSERA_MFT_CHUNK_TREE);
	if (outer == NULL) return (ENOMEM);

	/* Publish each group as a CHUNK_LIST sub-manifest pack, then add
	 * a tree_record entry to the outer manifest. */
	for (uint32_t g = 0; g < n_groups; g++) {
		const uint32_t chunk_first = g * fanout;
		const uint32_t chunk_last  = (chunk_first + fanout > n_chunks)
		    ? n_chunks : (chunk_first + fanout);
		const uint64_t group_off   = (uint64_t)chunk_first * cs;

		struct tessera_chunk_in *dirty = malloc(
		    (chunk_last - chunk_first) * sizeof(*dirty),
		    M_TESSERA, M_WAITOK | M_ZERO);
		uint32_t n_dirty = 0;

		tessera_manifest_builder_t *mb =
		    tessera_manifest_begin(TESSERA_MFT_CHUNK_LIST);
		if (mb == NULL) {
			free(dirty, M_TESSERA);
			tessera_manifest_free(outer);
			return (ENOMEM);
		}

		for (uint32_t i = chunk_first; i < chunk_last; i++) {
			const uint64_t off = (uint64_t)i * cs;
			const uint32_t len = (off + cs <= new_len) ? cs
			    : (uint32_t)(new_len - off);

			int all_zero = 1;
			for (uint32_t j = 0; j < len; j++) {
				if (new_bytes[off + j] != 0) {
					all_zero = 0;
					break;
				}
			}
			if (all_zero) {
				tessera_hash_t zh;
				memset(zh, 0, sizeof zh);
				if (tessera_manifest_add_chunk(mb, zh, off, len,
				    TESSERA_CHUNK_FLAG_ZERO_HOLE)
				    != TESSERA_OK) {
					tessera_manifest_free(mb);
					free(dirty, M_TESSERA);
					tessera_manifest_free(outer);
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
				tessera_manifest_free(outer);
				return (ENOMEM);
			}

			dirty[n_dirty].bytes = new_bytes + off;
			dirty[n_dirty].len   = len;
			memcpy(dirty[n_dirty].hash, h, sizeof h);
			n_dirty++;
		}

		size_t mlen = 0;
		tessera_hash_t mhash;
		(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
		uint8_t *mft = malloc(mlen, M_TESSERA, M_WAITOK);
		if (tessera_manifest_finalize(mb, mft, mlen, &mlen, mhash)
		    != TESSERA_OK) {
			tessera_manifest_free(mb);
			free(mft, M_TESSERA);
			free(dirty, M_TESSERA);
			tessera_manifest_free(outer);
			return (EIO);
		}
		tessera_manifest_free(mb);

		tessera_hash_t pub_hash;
		if (tessera_fs_publish_chunked(tmp_, dirty, n_dirty, mft,
		    mlen, pub_hash) != 0) {
			free(mft, M_TESSERA);
			free(dirty, M_TESSERA);
			tessera_manifest_free(outer);
			return (EIO);
		}
		free(mft, M_TESSERA);
		free(dirty, M_TESSERA);

		if (tessera_manifest_add_tree_child(outer, pub_hash,
		    group_off) != TESSERA_OK) {
			tessera_manifest_free(outer);
			return (ENOMEM);
		}
	}

	/* CHUNK_TREE total logical_size must equal the file size — the
	 * read path uses it to compute the last child's exclusive upper
	 * bound. add_tree_child only advances logical_size to the last
	 * entry's start_offset, so we must set it explicitly here. */
	(void)tessera_manifest_set_logical_size(outer, new_len);

	size_t olen = 0;
	tessera_hash_t ohash;
	(void)tessera_manifest_finalize(outer, NULL, 0, &olen, ohash);
	uint8_t *obuf = malloc(olen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(outer, obuf, olen, &olen, ohash)
	    != TESSERA_OK) {
		tessera_manifest_free(outer);
		free(obuf, M_TESSERA);
		return (EIO);
	}
	tessera_manifest_free(outer);

	/* Outer manifest is metadata-only — publish via the regular
	 * manifest path (with supersession-tagging by inode_no so a
	 * subsequent rewrite of the same file in the same flush window
	 * supersedes this entry rather than stacking). */
	if (tessera_fs_publish_manifest_owned(tmp_, obuf, olen, ohash,
	    inode_no) != 0) {
		free(obuf, M_TESSERA);
		return (EIO);
	}
	free(obuf, M_TESSERA);

	/* Update inode to point at the outer CHUNK_TREE manifest. */
	uint8_t key[4];
	encode_inode_key(inode_no, key);
	tessera_inode_record_t ino;
	if (tessera_fs_inode_get_byk(tmp_, key, &ino) != TESSERA_OK)
		return (EIO);
	memcpy(ino.manifest_hash, ohash, sizeof ohash);
	ino.size = new_len;
	ino.gen++;
	struct timeval tv;
	getmicrotime(&tv);
	uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
	    (uint64_t)tv.tv_usec * 1000ULL;
	ino.mtime_ns = ino.ctime_ns = now_ns;

	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_fs_inode_put_byk(tmp_, key, &ino,
	    &new_inode_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.inode_root = new_inode_root;

	tessera_stat_chunk_tree_publish++;
	return (0);
}

static int
tessera_fs_replace_content_chunked(struct tessera_mount *tmp_,
    uint32_t inode_no, const uint8_t *new_bytes, size_t new_len)
{
	if (new_len == 0) {
		/* Empty file = empty INLINE manifest, no chunks. */
		return tessera_fs_replace_content(tmp_, inode_no, new_bytes, 0);
	}

	/* Step-3c promotion: when the chunk count exceeds the fanout,
	 * spill into a CHUNK_TREE rather than a flat CHUNK_LIST. */
	{
		const uint32_t cs0 = tessera_chunk_size_for(tmp_, new_len);
		const uint32_t n0  = (uint32_t)((new_len + cs0 - 1) / cs0);
		if (n0 > TESSERA_CHUNK_TREE_FANOUT)
			return (tessera_fs_replace_content_chunk_tree(tmp_,
			    inode_no, new_bytes, new_len, cs0));
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
	if (tessera_fs_inode_get_byk(tmp_, key, &old_ino) != TESSERA_OK)
		return (EIO);

	const uint32_t cs = tessera_chunk_size_for(tmp_, new_len);

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
	if (tessera_fs_inode_get_byk(tmp_, key, &ino) != TESSERA_OK)
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
	if (tessera_fs_inode_put_byk(tmp_, key, &ino,
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
	if (tessera_fs_inode_get_byk(tmp_, key, &old_ino) != TESSERA_OK)
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
	const tessera_manifest_kind_t old_kind = tessera_manifest_parser_kind(p);

	/* CHUNK_TREE files have their own suffix-only fast-path (rebuilds
	 * just the last group + any spillover, keeps K-1 prefix tree
	 * records verbatim). The helper re-fetches the outer manifest. */
	if (old_kind == TESSERA_MFT_CHUNK_TREE) {
		tessera_manifest_parser_free(p);
		free(old_mft, M_TESSERA);
		return (tessera_fs_append_chunk_tree(tmp_, inode_no,
		    append_bytes, append_len, cs));
	}

	if (old_kind != TESSERA_MFT_CHUNK_LIST) {
		tessera_manifest_parser_free(p);
		free(old_mft, M_TESSERA);
		return (ENOTSUP);
	}

	/* Step-3c: a CHUNK_LIST file whose append would push it past the
	 * fanout must be promoted to CHUNK_TREE — bail to the slow path,
	 * which routes through replace_content_chunked and handles the
	 * promotion. Append fast-path stays flat-only here. */
	{
		const uint64_t projected_chunks = (new_size + cs - 1) / cs;
		if (projected_chunks > (uint64_t)TESSERA_CHUNK_TREE_FANOUT) {
			tessera_manifest_parser_free(p);
			free(old_mft, M_TESSERA);
			return (ENOTSUP);
		}
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
	if (tessera_fs_inode_get_byk(tmp_, key, &ino) != TESSERA_OK)
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
	if (tessera_fs_inode_put_byk(tmp_, key, &ino,
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
 * Append fast-path for CHUNK_TREE files (v2 step-3c follow-up).
 *
 * The flat-CHUNK_LIST append fast-path doesn't apply once a file has
 * been promoted to CHUNK_TREE. Without this helper, every append to a
 * CHUNK_TREE file would route through the slow whole-file rewrite —
 * which for a multi-GiB-at-4-KiB-cs file (e.g. a VM-image journal)
 * means materialising and re-hashing GiBs to add a few KiB. Defeats
 * the entire purpose of CHUNK_TREE.
 *
 * Suffix-only rewrite strategy:
 *
 *   - Outer CHUNK_TREE has K tree records pointing at K group
 *     sub-CHUNK_LISTs. The first K-1 groups (chunks 0..(K-1)*FANOUT-1)
 *     are unaffected by the append; their tree_records carry over
 *     verbatim into the new outer.
 *   - The last (tail) group is fetched and parsed. Its chunks
 *     0..M-2 keep their hashes verbatim. Its last chunk is
 *     materialised iff partial; merged with the head of the append.
 *   - Remaining append bytes are split into chunks and packed into
 *     the tail group up to FANOUT, then spill into newly-published
 *     groups until consumed.
 *   - New outer points at K-1 prefix records + 1 modified tail +
 *     0..M-1 spillover groups.
 *
 * Cost: O(append_len) bytes of chunking + O(K-1) tree_record copies
 * + O(spillover_groups + 1) sub-manifest republishes. The K-1
 * unmodified groups are free — their packs already exist on disk.
 *
 * 1 KiB log-line append to a 1 GiB-at-4-KiB-cs CHUNK_TREE file:
 * touches 1 chunk + the tail sub-manifest + the outer manifest.
 * ~50 KiB of write rather than ~1 GiB.
 *
 * Returns ENOTSUP for any malformed/mismatched input; caller falls
 * back to slow path (replace_content_chunked, which can rebuild from
 * raw bytes correctly even if structurally novel).
 */
static int
tessera_fs_append_chunk_tree(struct tessera_mount *tmp_, uint32_t inode_no,
    const uint8_t *append_bytes, size_t append_len, uint32_t cs)
{
	if (append_len == 0) return (ENOTSUP);

	uint8_t key[4];
	tessera_inode_record_t old_ino;
	encode_inode_key(inode_no, key);
	if (tessera_fs_inode_get_byk(tmp_, key, &old_ino) != TESSERA_OK)
		return (EIO);

	const uint64_t old_size = old_ino.size;
	if (old_size == 0) return (ENOTSUP);
	const uint64_t new_size = old_size + (uint64_t)append_len;

	/* Fetch + parse outer CHUNK_TREE manifest. */
	uint8_t  *outer_mft = NULL;
	uint32_t  outer_mlen = 0;
	if (tessera_fs_fetch_blob(tmp_, old_ino.manifest_hash,
	    &outer_mft, &outer_mlen) != 0)
		return (ENOTSUP);
	tessera_manifest_parser_t *op =
	    tessera_manifest_parse(outer_mft, outer_mlen);
	if (op == NULL ||
	    tessera_manifest_parser_kind(op) != TESSERA_MFT_CHUNK_TREE) {
		if (op) tessera_manifest_parser_free(op);
		free(outer_mft, M_TESSERA);
		return (ENOTSUP);
	}

	const uint32_t K = tessera_manifest_parser_count(op);
	if (K == 0) {
		tessera_manifest_parser_free(op);
		free(outer_mft, M_TESSERA);
		return (ENOTSUP);
	}

	/* Snapshot all K tree records. */
	struct tree_rec {
		tessera_hash_t hash;
		uint64_t       off;
	};
	struct tree_rec *otr = malloc(K * sizeof *otr, M_TESSERA,
	    M_WAITOK | M_ZERO);
	for (uint32_t i = 0; i < K; i++) {
		tessera_tree_record_t tr;
		if (tessera_manifest_tree_at(op, i, &tr) != TESSERA_OK) {
			tessera_manifest_parser_free(op);
			free(outer_mft, M_TESSERA);
			free(otr, M_TESSERA);
			return (ENOTSUP);
		}
		memcpy(otr[i].hash, tr.child_manifest_hash, sizeof tr.child_manifest_hash);
		otr[i].off = tr.logical_offset;
	}
	tessera_manifest_parser_free(op);
	free(outer_mft, M_TESSERA);

	const uint64_t tail_off = otr[K - 1].off;
	const uint64_t tail_byte_count = old_size - tail_off;
	if (tail_byte_count == 0) {
		free(otr, M_TESSERA);
		return (ENOTSUP);
	}

	/* Fetch + parse tail group's sub-CHUNK_LIST. */
	uint8_t  *tail_mft = NULL;
	uint32_t  tail_mlen = 0;
	if (tessera_fs_fetch_blob(tmp_, otr[K - 1].hash,
	    &tail_mft, &tail_mlen) != 0) {
		free(otr, M_TESSERA);
		return (ENOTSUP);
	}
	tessera_manifest_parser_t *tp =
	    tessera_manifest_parse(tail_mft, tail_mlen);
	if (tp == NULL ||
	    tessera_manifest_parser_kind(tp) != TESSERA_MFT_CHUNK_LIST) {
		if (tp) tessera_manifest_parser_free(tp);
		free(tail_mft, M_TESSERA);
		free(otr, M_TESSERA);
		return (ENOTSUP);
	}

	const uint32_t M = tessera_manifest_parser_count(tp);
	if (M == 0 || M > TESSERA_CHUNK_TREE_FANOUT) {
		tessera_manifest_parser_free(tp);
		free(tail_mft, M_TESSERA);
		free(otr, M_TESSERA);
		return (ENOTSUP);
	}

	/* Snapshot tail-group chunks. Validate offsets are global +
	 * full-cs sized except the last, which may be partial. */
	struct old_rec_t {
		tessera_hash_t hash;
		uint64_t       off;
		uint32_t       sz;
		uint32_t       flags;
	};
	struct old_rec_t *told = malloc(M * sizeof *told, M_TESSERA,
	    M_WAITOK | M_ZERO);
	int eligible = 1;
	for (uint32_t i = 0; i < M; i++) {
		tessera_chunk_record_t cr;
		if (tessera_manifest_chunk_at(tp, i, &cr) != TESSERA_OK ||
		    cr.logical_offset != tail_off + (uint64_t)i * cs ||
		    (i < M - 1 && cr.uncompressed_size != cs)) {
			eligible = 0;
			break;
		}
		memcpy(told[i].hash, cr.chunk_hash, sizeof cr.chunk_hash);
		told[i].off   = cr.logical_offset;
		told[i].sz    = cr.uncompressed_size;
		told[i].flags = cr.flags;
	}
	tessera_manifest_parser_free(tp);
	free(tail_mft, M_TESSERA);
	if (!eligible) {
		free(told, M_TESSERA);
		free(otr, M_TESSERA);
		return (ENOTSUP);
	}

	const uint32_t last_old_sz = told[M - 1].sz;
	const int last_partial = (last_old_sz < cs);

	/* Materialise old last chunk if partial (need it for merging
	 * with the head of the append). */
	uint8_t *merge_buf = NULL;
	if (last_partial) {
		merge_buf = malloc(cs, M_TESSERA, M_WAITOK | M_ZERO);
		if (!(told[M - 1].flags & TESSERA_CHUNK_FLAG_ZERO_HOLE)) {
			uint8_t *cb = NULL;
			uint32_t cb_len = 0;
			if (tessera_fs_fetch_blob(tmp_, told[M - 1].hash,
			    &cb, &cb_len) != 0) {
				free(merge_buf, M_TESSERA);
				free(told, M_TESSERA);
				free(otr, M_TESSERA);
				return (ENOTSUP);
			}
			memcpy(merge_buf, cb,
			    (last_old_sz < cb_len) ? last_old_sz : cb_len);
			free(cb, M_TESSERA);
		}
	}

	/* Build the new outer manifest. K-1 prefix tree records carry
	 * over verbatim. We then publish modified tail + spillover
	 * groups, adding tree records as we go. */
	tessera_manifest_builder_t *outer =
	    tessera_manifest_begin(TESSERA_MFT_CHUNK_TREE);
	if (outer == NULL) {
		if (merge_buf) free(merge_buf, M_TESSERA);
		free(told, M_TESSERA);
		free(otr, M_TESSERA);
		return (ENOMEM);
	}
	for (uint32_t i = 0; i + 1 < K; i++) {
		if (tessera_manifest_add_tree_child(outer, otr[i].hash,
		    otr[i].off) != TESSERA_OK) {
			tessera_manifest_free(outer);
			if (merge_buf) free(merge_buf, M_TESSERA);
			free(told, M_TESSERA);
			free(otr, M_TESSERA);
			return (ENOMEM);
		}
	}

	/* Walk new bytes group-by-group. Each group is a CHUNK_LIST
	 * pack publish; outer references its hash. */
	size_t append_pos = 0;
	uint64_t cur_group_off = tail_off;
	const uint32_t SOFT_CAP = TESSERA_CHUNK_TREE_FANOUT;

	/* For the first iteration the group seeds with tail_old chunks
	 * (0..M-2 verbatim + modified last). Subsequent spillover groups
	 * start empty. */
	int seeded_with_tail = 1;

	while (append_pos < append_len ||
	    (seeded_with_tail && M > 0)) {
		struct tessera_chunk_in *dirty = malloc(
		    SOFT_CAP * sizeof *dirty, M_TESSERA, M_WAITOK | M_ZERO);
		uint32_t n_dirty = 0;

		tessera_manifest_builder_t *mb =
		    tessera_manifest_begin(TESSERA_MFT_CHUNK_LIST);
		if (mb == NULL) {
			free(dirty, M_TESSERA);
			tessera_manifest_free(outer);
			if (merge_buf) free(merge_buf, M_TESSERA);
			free(told, M_TESSERA);
			free(otr, M_TESSERA);
			return (ENOMEM);
		}

		uint32_t group_chunks = 0;
		uint64_t cur_off = cur_group_off;

		if (seeded_with_tail) {
			/* Carry chunks 0..M-2 verbatim. */
			for (uint32_t i = 0; i + 1 < M; i++) {
				if (tessera_manifest_add_chunk(mb,
				    told[i].hash, told[i].off, told[i].sz,
				    told[i].flags) != TESSERA_OK)
					goto et_enomem;
				group_chunks++;
				cur_off = told[i].off + told[i].sz;
			}
			/* Modified last chunk: if partial, merge with append
			 * head; else if aligned, the "old last chunk" was
			 * already full and gets carried verbatim too. */
			if (last_partial) {
				const uint32_t fill =
				    (uint32_t)(cs - last_old_sz);
				const uint32_t take = (append_len - append_pos
				    < (size_t)fill) ?
				    (uint32_t)(append_len - append_pos) : fill;
				memcpy(merge_buf + last_old_sz,
				    append_bytes + append_pos, take);
				const uint32_t merged_sz = last_old_sz + take;

				int all_zero = 1;
				for (uint32_t j = 0; j < merged_sz; j++)
					if (merge_buf[j] != 0) {
						all_zero = 0;
						break;
					}
				if (all_zero) {
					tessera_hash_t zh;
					memset(zh, 0, sizeof zh);
					if (tessera_manifest_add_chunk(mb, zh,
					    told[M - 1].off, merged_sz,
					    TESSERA_CHUNK_FLAG_ZERO_HOLE)
					    != TESSERA_OK) goto et_enomem;
					tessera_stat_chunk_zero_hole++;
				} else {
					tessera_hash_t h;
					tessera_sha256(merge_buf, merged_sz, h);
					if (tessera_manifest_add_chunk(mb, h,
					    told[M - 1].off, merged_sz, 0)
					    != TESSERA_OK) goto et_enomem;
					dirty[n_dirty].bytes = merge_buf;
					dirty[n_dirty].len   = merged_sz;
					memcpy(dirty[n_dirty].hash, h, sizeof h);
					n_dirty++;
				}
				group_chunks++;
				cur_off = told[M - 1].off + merged_sz;
				append_pos += take;
			} else {
				/* Aligned EOF: old last chunk is full-cs and
				 * untouched. Carry verbatim. */
				if (tessera_manifest_add_chunk(mb,
				    told[M - 1].hash, told[M - 1].off,
				    told[M - 1].sz, told[M - 1].flags)
				    != TESSERA_OK) goto et_enomem;
				group_chunks++;
				cur_off = told[M - 1].off + told[M - 1].sz;
			}
			seeded_with_tail = 0;
		}

		/* Fill remainder of this group with new appended chunks. */
		while (group_chunks < SOFT_CAP && append_pos < append_len) {
			const size_t remain = append_len - append_pos;
			const uint32_t this_len = (remain >= (size_t)cs)
			    ? cs : (uint32_t)remain;

			int all_zero = 1;
			for (uint32_t j = 0; j < this_len; j++) {
				if (append_bytes[append_pos + j] != 0) {
					all_zero = 0;
					break;
				}
			}
			if (all_zero) {
				tessera_hash_t zh;
				memset(zh, 0, sizeof zh);
				if (tessera_manifest_add_chunk(mb, zh, cur_off,
				    this_len, TESSERA_CHUNK_FLAG_ZERO_HOLE)
				    != TESSERA_OK) goto et_enomem;
				tessera_stat_chunk_zero_hole++;
			} else {
				tessera_hash_t h;
				tessera_sha256(append_bytes + append_pos,
				    this_len, h);
				if (tessera_manifest_add_chunk(mb, h, cur_off,
				    this_len, 0) != TESSERA_OK)
					goto et_enomem;
				dirty[n_dirty].bytes =
				    append_bytes + append_pos;
				dirty[n_dirty].len   = this_len;
				memcpy(dirty[n_dirty].hash, h, sizeof h);
				n_dirty++;
			}
			group_chunks++;
			append_pos += this_len;
			cur_off    += this_len;
		}

		/* Finalize + publish this group. */
		size_t mlen = 0;
		tessera_hash_t mhash;
		(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
		uint8_t *mft = malloc(mlen, M_TESSERA, M_WAITOK);
		if (tessera_manifest_finalize(mb, mft, mlen, &mlen, mhash)
		    != TESSERA_OK) {
			free(mft, M_TESSERA);
			goto et_enomem;
		}
		tessera_manifest_free(mb);
		mb = NULL;

		tessera_hash_t pub_hash;
		if (tessera_fs_publish_chunked(tmp_, dirty, n_dirty, mft,
		    mlen, pub_hash) != 0) {
			free(mft, M_TESSERA);
			free(dirty, M_TESSERA);
			tessera_manifest_free(outer);
			if (merge_buf) free(merge_buf, M_TESSERA);
			free(told, M_TESSERA);
			free(otr, M_TESSERA);
			return (EIO);
		}
		free(mft, M_TESSERA);
		free(dirty, M_TESSERA);

		if (tessera_manifest_add_tree_child(outer, pub_hash,
		    cur_group_off) != TESSERA_OK) {
			tessera_manifest_free(outer);
			if (merge_buf) free(merge_buf, M_TESSERA);
			free(told, M_TESSERA);
			free(otr, M_TESSERA);
			return (ENOMEM);
		}

		/* Next group starts at cur_off (the byte position we've
		 * advanced to). */
		cur_group_off = cur_off;
		continue;
et_enomem:
		if (mb) tessera_manifest_free(mb);
		free(dirty, M_TESSERA);
		tessera_manifest_free(outer);
		if (merge_buf) free(merge_buf, M_TESSERA);
		free(told, M_TESSERA);
		free(otr, M_TESSERA);
		return (ENOMEM);
	}

	if (merge_buf) free(merge_buf, M_TESSERA);
	free(told, M_TESSERA);
	free(otr, M_TESSERA);

	/* Outer's logical_size must equal new file size (read path uses
	 * it as the last subtree's exclusive upper bound). */
	(void)tessera_manifest_set_logical_size(outer, new_size);

	size_t olen = 0;
	tessera_hash_t ohash;
	(void)tessera_manifest_finalize(outer, NULL, 0, &olen, ohash);
	uint8_t *obuf = malloc(olen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(outer, obuf, olen, &olen, ohash)
	    != TESSERA_OK) {
		tessera_manifest_free(outer);
		free(obuf, M_TESSERA);
		return (EIO);
	}
	tessera_manifest_free(outer);

	if (tessera_fs_publish_manifest_owned(tmp_, obuf, olen, ohash,
	    inode_no) != 0) {
		free(obuf, M_TESSERA);
		return (EIO);
	}
	free(obuf, M_TESSERA);

	tessera_inode_record_t ino;
	if (tessera_fs_inode_get_byk(tmp_, key, &ino) != TESSERA_OK)
		return (EIO);
	memcpy(ino.manifest_hash, ohash, sizeof ohash);
	ino.size = new_size;
	ino.gen++;
	struct timeval tv;
	getmicrotime(&tv);
	uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
	    (uint64_t)tv.tv_usec * 1000ULL;
	ino.mtime_ns = ino.ctime_ns = now_ns;

	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_fs_inode_put_byk(tmp_, key, &ino,
	    &new_inode_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.inode_root = new_inode_root;

	tessera_stat_chunk_tree_publish++;
	return (0);
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

/*
 * Sticky-bit check (POSIX): in a directory whose mode includes
 * S_ISVTX, a file may be unlinked or renamed only by:
 *   - root (PRIV_VFS_ADMIN)
 *   - the owner of the directory
 *   - the owner of the file
 * Returns 0 if allowed, EPERM otherwise. UFS does this in
 * ufs_dir_check_path / sticky checks scattered through unlink/rename.
 */
static int
tessera_fs_sticky_check(struct tessera_mount *tmp_,
                        struct tessera_node *dn, struct tessera_node *cn,
                        struct ucred *cred)
{
	uint8_t k[4];
	tessera_inode_record_t dino, cino;
	encode_inode_key((uint32_t)dn->inode_no, k);
	if (tessera_fs_inode_get_byk(tmp_, k, &dino) != TESSERA_OK)
		return (0);
	if ((dino.mode & S_ISVTX) == 0) return (0);
	encode_inode_key((uint32_t)cn->inode_no, k);
	if (tessera_fs_inode_get_byk(tmp_, k, &cino) != TESSERA_OK)
		return (0);
	if (cred->cr_uid == 0) return (0);
	if (cred->cr_uid == dino.uid) return (0);
	if (cred->cr_uid == cino.uid) return (0);
	if (priv_check_cred(cred, PRIV_VFS_ADMIN) == 0) return (0);
	return (EPERM);
}

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
	{
		int aerr = VOP_ACCESS(dvp, VWRITE, cnp->cn_cred, curthread);
		if (aerr != 0) return (aerr);
	}
	{
		int serr = tessera_fs_sticky_check(tmp_, dn, cn, cnp->cn_cred);
		if (serr != 0) return (serr);
	}

	/* dirent_rewrite handles flat DIRECTORY and DIRECTORY_2L parents
	 * uniformly via dir_walk + auto-promoting publish_directory. */
	int err = tessera_fs_dirent_rewrite(tmp_,
	    (uint32_t)dn->inode_no,
	    /*op=REMOVE*/ 1, /*verify*/ cn->inode_no,
	    /*add_inode*/ 0,
	    cnp->cn_nameptr, cnp->cn_namelen);
	if (err != 0) return (err);

	/* Drop a link on the child. tessera_fs_inode_unlink decrements
	 * nlink; only btree_deletes the record when it hits 0, so
	 * hardlinks survive. */
	(void)tessera_fs_inode_unlink(tmp_, (uint32_t)cn->inode_no);

	tessera_fs_mark_dirty(tmp_);
	return (0);
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
	} else if (kind == TESSERA_MFT_CHUNK_LIST ||
	           kind == TESSERA_MFT_CHUNK_TREE) {
		/* Synthesize a kernel-space uio over the destination buffer
		 * and reuse the recursive read helper that vop_read uses.
		 * Handles flat CHUNK_LIST and recursive CHUNK_TREE
		 * uniformly, including ZERO_HOLE chunks. */
		struct uio _uio;
		struct iovec _iov;
		_iov.iov_base   = buf;
		_iov.iov_len    = (size_t)ino->size;
		_uio.uio_iov    = &_iov;
		_uio.uio_iovcnt = 1;
		_uio.uio_offset = 0;
		_uio.uio_resid  = (ssize_t)ino->size;
		_uio.uio_segflg = UIO_SYSSPACE;
		_uio.uio_rw     = UIO_READ;
		_uio.uio_td     = curthread;
		err = tessera_fs_read_into_uio(tmp_, p, &_uio);
	} else {
		err = EIO;  /* SYMLINK / DIRECTORY not handled here */
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
	if (tessera_fs_publish_manifest_owned(tmp_, mft, mlen, pub_hash,
	    inode_no) != 0) {
		free(mft, M_TESSERA);
		return (EIO);
	}
	free(mft, M_TESSERA);

	uint8_t key[4];
	tessera_inode_record_t ino;
	encode_inode_key(inode_no, key);
	if (tessera_fs_inode_get_byk(tmp_, key, &ino) != TESSERA_OK)
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
	if (tessera_fs_inode_put_byk(tmp_, key, &ino,
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
/* Per-iteration ctx for the dir_walk callback used by dirent_rewrite. */
struct dirent_rewrite_ctx {
	tessera_manifest_builder_t *mb;
	int       op;                /* 0=ADD, 1=REMOVE */
	uint64_t  verify_inode;
	const char *skip_name;
	size_t    skip_namelen;
	int       matched;
	int       err;
};

static int
dirent_rewrite_visit(void *vctx, uint64_t child_inode,
                     const char *name, uint16_t name_len)
{
	struct dirent_rewrite_ctx *c = vctx;
	int match = (name_len == c->skip_namelen) &&
	    (memcmp(name, c->skip_name, c->skip_namelen) == 0);
	if (match) {
		c->matched = 1;
		if (c->op == 0) { c->err = EEXIST; return (EEXIST); }
		if (c->verify_inode != 0 &&
		    child_inode != c->verify_inode) {
			c->err = EIO; return (EIO);
		}
		return (0);  /* skip — REMOVE */
	}
	if (tessera_manifest_add_dirent(c->mb, child_inode, name,
	    name_len) != TESSERA_OK) {
		c->err = ENOMEM; return (ENOMEM);
	}
	return (0);
}

/*
 * 2L-aware fast path. dirent_rewrite_2l touches only the affected
 * bucket and rewrites the outer manifest with one bucket entry
 * changed; per-op cost is O(bucket-size + K) instead of O(N).
 *
 * Without this, a 500-entry parent dir gets its full ~50 KiB
 * manifest read + walked + rewritten on every add/remove, so e.g.
 * `rm` of one entry costs ~14 ms. With the fast path, only the
 * matching bucket (~N/16 entries, typically a few sectors) gets
 * rewritten — same operation drops to under 1 ms.
 */
static int
tessera_fs_dirent_rewrite_2l(struct tessera_mount *tmp_,
                             uint32_t parent_inode_no,
                             tessera_inode_record_t *pino,
                             int op, uint64_t verify_inode,
                             uint64_t add_inode,
                             const char *name, size_t namelen)
{
	uint8_t  *outer_blob = NULL;
	uint32_t  outer_blen = 0;
	if (tessera_fs_fetch_blob(tmp_, pino->manifest_hash,
	    &outer_blob, &outer_blen) != 0) return (EIO);
	if (outer_blen < 32) { free(outer_blob, M_TESSERA); return (EIO); }
	tessera_manifest_parser_t *outer =
	    tessera_manifest_parse(outer_blob, outer_blen);
	if (outer == NULL) {
		free(outer_blob, M_TESSERA);
		return (EIO);
	}

	const uint64_t name_h = tessera_dir_name_hash(name, (uint16_t)namelen);
	const uint32_t nbk = tessera_manifest_parser_count(outer);

	/* Find the bucket that would contain `name`: largest first_hash ≤
	 * name_h. Same logic as tessera_fs_dir_2l_lookup. */
	int target_idx = -1;
	{
		int lo = 0, hi = (int)nbk - 1, best = 0;
		while (lo <= hi) {
			int mid = lo + (hi - lo) / 2;
			tessera_dir_bucket_record_t br;
			if (tessera_manifest_dir_bucket_at(outer,
			    (uint32_t)mid, &br) != TESSERA_OK) {
				tessera_manifest_parser_free(outer);
				free(outer_blob, M_TESSERA);
				return (EIO);
			}
			if (br.first_name_hash <= name_h) {
				best = mid;
				lo = mid + 1;
			} else {
				hi = mid - 1;
			}
		}
		if (nbk > 0) target_idx = best;
	}

	/* Fetch the target bucket (if any) and walk it once to apply the
	 * REMOVE skip / detect EEXIST for ADD. Build a fresh bucket
	 * builder while we go. */
	tessera_manifest_builder_t *bmb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
	if (bmb == NULL) {
		tessera_manifest_parser_free(outer);
		free(outer_blob, M_TESSERA);
		return (ENOMEM);
	}

	int matched = 0;
	uint64_t bucket_first_hash = name_h;
	uint32_t bucket_count = 0;
	uint8_t  *bbuf = NULL;
	uint32_t  blen = 0;

	if (target_idx >= 0) {
		tessera_dir_bucket_record_t target;
		if (tessera_manifest_dir_bucket_at(outer,
		    (uint32_t)target_idx, &target) != TESSERA_OK) {
			tessera_manifest_free(bmb);
			tessera_manifest_parser_free(outer);
			free(outer_blob, M_TESSERA);
			return (EIO);
		}
		if (tessera_fs_fetch_blob(tmp_, target.bucket_manifest_hash,
		    &bbuf, &blen) != 0) {
			tessera_manifest_free(bmb);
			tessera_manifest_parser_free(outer);
			free(outer_blob, M_TESSERA);
			return (EIO);
		}
		if (blen < 32) {
			free(bbuf, M_TESSERA);
			tessera_manifest_free(bmb);
			tessera_manifest_parser_free(outer);
			free(outer_blob, M_TESSERA);
			return (EIO);
		}
		const uint8_t *body = bbuf + 32;
		const size_t   body_len = blen - 32;
		int saw_first = 0;
		for (size_t off = 0; off + 10 <= body_len; ) {
			uint64_t ch;
			uint16_t nl;
			memcpy(&ch, body + off,     8);
			memcpy(&nl, body + off + 8, 2);
			if (off + 10 + nl > body_len) {
				free(bbuf, M_TESSERA);
				tessera_manifest_free(bmb);
				tessera_manifest_parser_free(outer);
				free(outer_blob, M_TESSERA);
				return (EIO);
			}
			const char *nm = (const char *)(body + off + 10);
			int match = (nl == namelen) &&
			    (memcmp(nm, name, namelen) == 0);
			if (match) {
				matched = 1;
				if (op == 0) { /* ADD: collision */
					free(bbuf, M_TESSERA);
					tessera_manifest_free(bmb);
					tessera_manifest_parser_free(outer);
					free(outer_blob, M_TESSERA);
					return (EEXIST);
				}
				if (verify_inode != 0 &&
				    ch != verify_inode) {
					free(bbuf, M_TESSERA);
					tessera_manifest_free(bmb);
					tessera_manifest_parser_free(outer);
					free(outer_blob, M_TESSERA);
					return (EIO);
				}
				/* Skip — REMOVE drops it. */
			} else {
				uint64_t this_h = tessera_dir_name_hash(nm, nl);
				if (!saw_first || this_h < bucket_first_hash) {
					bucket_first_hash = this_h;
					saw_first = 1;
				}
				if (tessera_manifest_add_dirent(bmb, ch, nm,
				    nl) != TESSERA_OK) {
					free(bbuf, M_TESSERA);
					tessera_manifest_free(bmb);
					tessera_manifest_parser_free(outer);
					free(outer_blob, M_TESSERA);
					return (ENOMEM);
				}
				bucket_count++;
			}
			off += 10 + nl;
		}
		free(bbuf, M_TESSERA);
	}

	if (op == 1 && !matched) {
		tessera_manifest_free(bmb);
		tessera_manifest_parser_free(outer);
		free(outer_blob, M_TESSERA);
		return (ENOENT);
	}
	if (op == 0) {
		if (tessera_manifest_add_dirent(bmb, add_inode, name,
		    (uint16_t)namelen) != TESSERA_OK) {
			tessera_manifest_free(bmb);
			tessera_manifest_parser_free(outer);
			free(outer_blob, M_TESSERA);
			return (ENOMEM);
		}
		if (bucket_count == 0 || name_h < bucket_first_hash)
			bucket_first_hash = name_h;
		bucket_count++;
	}

	/* Publish the new bucket. Skip publish if it's now empty —
	 * we'll just drop it from the outer below. */
	tessera_hash_t new_bucket_hash;
	int bucket_dropped = (bucket_count == 0);
	if (!bucket_dropped) {
		size_t bmlen = 0;
		tessera_hash_t bmhash;
		(void)tessera_manifest_finalize(bmb, NULL, 0, &bmlen, bmhash);
		uint8_t *bbody = malloc(bmlen, M_TESSERA, M_WAITOK);
		if (tessera_manifest_finalize(bmb, bbody, bmlen, &bmlen,
		    bmhash) != TESSERA_OK) {
			free(bbody, M_TESSERA);
			tessera_manifest_free(bmb);
			tessera_manifest_parser_free(outer);
			free(outer_blob, M_TESSERA);
			return (EIO);
		}
		/* Owner=0: bucket bytes are referenced by the outer
		 * manifest. If we superseded by parent_inode_no the
		 * bucket eviction would race the outer publish and
		 * leave the outer briefly pointing at evicted bytes.
		 * Untagged entries accumulate until commit_sb drains
		 * them to disk; the pending-manifest byte cap drives
		 * flush cadence. */
		if (tessera_fs_publish_manifest(tmp_, bbody, bmlen,
		    new_bucket_hash) != 0) {
			free(bbody, M_TESSERA);
			tessera_manifest_free(bmb);
			tessera_manifest_parser_free(outer);
			free(outer_blob, M_TESSERA);
			return (EIO);
		}
		free(bbody, M_TESSERA);
	}
	tessera_manifest_free(bmb);

	/* Build the new outer: copy unchanged buckets, replace the
	 * target one with the rewritten bucket (or drop if empty). */
	tessera_manifest_builder_t *omb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY_2L);
	if (omb == NULL) {
		tessera_manifest_parser_free(outer);
		free(outer_blob, M_TESSERA);
		return (ENOMEM);
	}
	int placed_new = 0;
	for (uint32_t i = 0; i < nbk; i++) {
		tessera_dir_bucket_record_t br;
		if (tessera_manifest_dir_bucket_at(outer, i, &br)
		    != TESSERA_OK) {
			tessera_manifest_free(omb);
			tessera_manifest_parser_free(outer);
			free(outer_blob, M_TESSERA);
			return (EIO);
		}
		if ((int)i == target_idx) {
			if (bucket_dropped) continue;
			/* Place the rewritten bucket. With top-4-bits
			 * (h >> 60) bucket assignment each bucket owns a
			 * disjoint hash range, so the slot stays in
			 * position regardless of which entry now has the
			 * smallest hash within it. */
			if (tessera_manifest_add_dir_bucket(omb,
			    bucket_first_hash, new_bucket_hash)
			    != TESSERA_OK) {
				tessera_manifest_free(omb);
				tessera_manifest_parser_free(outer);
				free(outer_blob, M_TESSERA);
				return (ENOMEM);
			}
			placed_new = 1;
		} else {
			if (tessera_manifest_add_dir_bucket(omb,
			    br.first_name_hash, br.bucket_manifest_hash)
			    != TESSERA_OK) {
				tessera_manifest_free(omb);
				tessera_manifest_parser_free(outer);
				free(outer_blob, M_TESSERA);
				return (ENOMEM);
			}
		}
	}
	if (op == 0 && !placed_new && !bucket_dropped) {
		/* ADD into a bucket that didn't exist — first entry
		 * with this top-4-bit prefix. Append; outer was sorted
		 * by first_name_hash so this may be out of order, but
		 * binary-search-by-hash continues to work because each
		 * bucket's hash range is disjoint from its siblings. */
		if (tessera_manifest_add_dir_bucket(omb,
		    bucket_first_hash, new_bucket_hash) != TESSERA_OK) {
			tessera_manifest_free(omb);
			tessera_manifest_parser_free(outer);
			free(outer_blob, M_TESSERA);
			return (ENOMEM);
		}
	}
	tessera_manifest_parser_free(outer);
	free(outer_blob, M_TESSERA);

	size_t omlen = 0;
	tessera_hash_t omhash;
	(void)tessera_manifest_finalize(omb, NULL, 0, &omlen, omhash);
	uint8_t *obody = malloc(omlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(omb, obody, omlen, &omlen, omhash)
	    != TESSERA_OK) {
		free(obody, M_TESSERA);
		tessera_manifest_free(omb);
		return (EIO);
	}
	tessera_manifest_free(omb);

	tessera_hash_t pub_hash;
	if (tessera_fs_publish_manifest_owned(tmp_, obody, omlen,
	    pub_hash, parent_inode_no) != 0) {
		free(obody, M_TESSERA);
		return (EIO);
	}
	free(obody, M_TESSERA);

	memcpy(pino->manifest_hash, pub_hash, sizeof pub_hash);
	return (0);
}

/* ──────────────────────────────────────────────────────────────────
 * v2.6 dirent log: log-structured buffering of pending dirent ops.
 *
 * Each dirent_rewrite caller can route through the log instead of
 * immediately mutating the parent's BTREE (gated by
 * kern.tessera.dirent_log_enable). The log records stay in RAM
 * until checkpoint:
 *   - vop_readdir on parent forces a checkpoint of that parent
 *     before walking BTREE.
 *   - tessera_fs_flush calls checkpoint_all_dirty before commit_sb.
 *   - mark_dirty arms a flush when log_count crosses the threshold.
 *
 * Read merge (vop_lookup): consult log for (parent, name) → return
 * the most-recent ADD's inode_no, or ENOENT for REMOVE, else fall
 * through to BTREE descent.
 *
 * Per-op cost: list append (~µs).
 * Per-checkpoint cost: O(N + K) per dirty parent — walk current
 * BTREE, merge log entries, bulk-build new BTREE in one pass. K
 * dirent ops on the same parent collapse to ONE BTREE rebuild
 * instead of K successive COW updates.
 *
 * Crash safety: log is RAM-only initially. fsync triggers
 * checkpoint → BTREE update → commit_sb. Same durability as the
 * existing dirty_inodes cache. (Phase B.2 puts log records on the
 * on-disk journal for recovery without explicit fsync.)
 *
 * CAS preservation: the on-disk state — pack_registry entries,
 * BTREE manifests, snapshot roots — is fully content-addressed.
 * The log just buffers a batch of dirent ops into a single CAS
 * publish per affected parent.
 * ────────────────────────────────────────────────────────────────── */

/* Definition matches the forward decl up-top. Default ON so the
 * fast path is exercised by the existing test/bench harness; flip
 * via sysctl to fall back to the immediate-BTREE-update path. */
static int tessera_dirent_log_enable_default = 1;
SYSCTL_INT(_kern_tessera, OID_AUTO, dirent_log_enable,
    CTLFLAG_RW, &tessera_dirent_log_enable_default, 0,
    "1 = route dirent ops through the v2.6 log instead of immediate BTREE update");

static int tessera_dirent_log_threshold = 1024;
SYSCTL_INT(_kern_tessera, OID_AUTO, dirent_log_threshold,
    CTLFLAG_RW, &tessera_dirent_log_threshold, 0,
    "Force flush when dirent_log_count exceeds this");

/* Append (parent, op, name, inode_no) to the log. Caller holds no
 * lock; we take flush_mtx briefly. */
static int
tessera_fs_dirent_log_append(struct tessera_mount *tmp_,
    uint32_t parent_inode_no, int op,
    const char *name, uint16_t namelen, uint64_t inode_no)
{
	if (!tmp_->dirty_init) return (EIO);
	if (namelen == 0 || namelen > TESSERA_PATH_NAME_MAX)
		return (EINVAL);
	size_t sz = sizeof(struct tessera_dirent_log_entry) + namelen;
	struct tessera_dirent_log_entry *e = malloc(sz, M_TESSERA, M_WAITOK);
	e->parent_inode_no = parent_inode_no;
	e->inode_no        = (uint32_t)inode_no;
	e->name_hash       = tessera_dir_name_hash(name, namelen);
	e->op              = (uint8_t)op;
	e->name_len        = namelen;
	memcpy(e->name, name, namelen);

	/* Pre-allocate the journal-pending clone OUTSIDE the lock —
	 * tessera_dirent_log_entry_clone uses M_WAITOK, which is
	 * disallowed while holding flush_mtx (a sleep mutex; under
	 * INVARIANTS the kernel panics with "malloc(M_WAITOK) with
	 * sleeping prohibited"). */
	struct tessera_dirent_log_entry *jc = NULL;
	if (tessera_journal_log_enable_default && tmp_->journal != NULL && !tmp_->in_replay)
		jc = tessera_dirent_log_entry_clone(e);

	mtx_lock(&tmp_->flush_mtx);
	e->seq = tmp_->dirent_log_seq++;
	uint32_t b = parent_inode_no & (TESSERA_DIRENT_LOG_BUCKETS - 1u);
	LIST_INSERT_HEAD(&tmp_->dirent_log[b], e, link);
	tmp_->dirent_log_count++;
	/* v2.6 Phase B.2: enqueue the pre-built clone onto the
	 * journal-pending list. The group-commit callout drains it
	 * into a single tx every journal_log_interval_ms; on crash +
	 * remount the journaled records re-create the in-memory log. */
	if (jc != NULL) {
		jc->seq = e->seq;
		LIST_INSERT_HEAD(&tmp_->journal_pending, jc, link);
		tmp_->journal_pending_count++;
	}
	mtx_unlock(&tmp_->flush_mtx);

	/* Bump the parent's mtime/ctime in the dirty inodes cache so
	 * an intervening stat sees fresh timestamps without forcing a
	 * full checkpoint. inode_put coalesces by inode_no so this is
	 * one entry regardless of how many ops we accumulate before
	 * checkpoint. POSIX requires this — pjdfstest mkdir/00,
	 * symlink/00 etc. assert the parent's mtime advances after the
	 * dir-modifying op. */
	{
		uint8_t pkey[4];
		tessera_inode_record_t pino;
		encode_inode_key(parent_inode_no, pkey);
		if (tessera_fs_inode_get_byk(tmp_, pkey, &pino) == TESSERA_OK) {
			struct timeval tv;
			getmicrotime(&tv);
			uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
			    (uint64_t)tv.tv_usec * 1000ULL;
			pino.mtime_ns = now_ns;
			pino.ctime_ns = now_ns;
			(void)tessera_fs_inode_put_byk(tmp_, pkey, &pino, NULL);
		}
	}
	return (0);
}

/* Lookup (parent, name) in the log. Returns:
 *   0 + *out_inode set + *out_op set: most-recent op found.
 *   TESSERA_DIRENT_LOG_MISS: no entry — caller falls through to
 *   BTREE. (Defined alongside the forward decl up top.)
 */
static int
tessera_fs_dirent_log_lookup(struct tessera_mount *tmp_,
    uint32_t parent_inode_no, const char *name, uint16_t namelen,
    int *out_op, uint64_t *out_inode)
{
	if (!tmp_->dirty_init) return (TESSERA_DIRENT_LOG_MISS);
	uint32_t b = parent_inode_no & (TESSERA_DIRENT_LOG_BUCKETS - 1u);
	mtx_lock(&tmp_->flush_mtx);
	struct tessera_dirent_log_entry *e, *latest = NULL;
	LIST_FOREACH(e, &tmp_->dirent_log[b], link) {
		if (e->parent_inode_no != parent_inode_no) continue;
		if (e->name_len != namelen) continue;
		if (memcmp(e->name, name, namelen) != 0) continue;
		if (latest == NULL || e->seq > latest->seq) latest = e;
	}
	if (latest == NULL) {
		mtx_unlock(&tmp_->flush_mtx);
		return (TESSERA_DIRENT_LOG_MISS);
	}
	*out_op    = latest->op;
	*out_inode = latest->inode_no;
	mtx_unlock(&tmp_->flush_mtx);
	return (0);
}

/* Bulk-build a balanced BTREE from a sorted array. Same shape as
 * tessera_fs_dir_btree_migrate's tail (the layer-by-layer build),
 * extracted here so checkpoint can reuse it without going through
 * dir_walk. Caller MUST sort `hashes` ascending. */
static int
tessera_fs_dir_btree_bulk_build(struct tessera_mount *tmp_,
    const uint64_t *hashes, const uint64_t *inos,
    const char *const *names, const uint16_t *nlens,
    uint32_t count, tessera_hash_t out_root)
{
	if (count == 0) {
		/* Empty dir → publish a zero-entry leaf. */
		tessera_manifest_builder_t *mb =
		    tessera_manifest_begin(TESSERA_MFT_DIRECTORY_BTREE);
		if (mb == NULL) return (ENOMEM);
		tessera_manifest_dir_btree_set_leaf(mb, 1);
		size_t mlen = 0;
		tessera_hash_t mh;
		(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mh);
		uint8_t *buf = malloc(mlen, M_TESSERA, M_WAITOK);
		(void)tessera_manifest_finalize(mb, buf, mlen, &mlen, mh);
		tessera_manifest_free(mb);
		int prc = tessera_fs_publish_manifest_owned_known_new(tmp_,
		    buf, mlen, out_root, /*owner=*/0);
		free(buf, M_TESSERA);
		return (prc == 0 ? 0 : EIO);
	}

	uint32_t F = TESSERA_DIR_BTREE_FANOUT_LEAF;
	uint32_t leaf_count = (count + F - 1) / F;
	tessera_hash_t *layer = malloc(leaf_count * sizeof *layer,
	    M_TESSERA, M_WAITOK);
	uint64_t *layer_max = malloc(leaf_count * sizeof *layer_max,
	    M_TESSERA, M_WAITOK);
	int rc = 0;

	for (uint32_t i = 0; i < leaf_count; i++) {
		uint32_t start = i * F;
		uint32_t take = count - start;
		if (take > F) take = F;
		rc = tessera_fs_dir_btree_publish_leaf(tmp_,
		    hashes + start, inos + start, names + start,
		    nlens + start, take, layer[i]);
		if (rc != 0) goto out;
		layer_max[i] = hashes[start + take - 1];
	}

	uint32_t IF = TESSERA_DIR_BTREE_FANOUT_INNER;
	while (leaf_count > 1) {
		uint32_t up = (leaf_count + IF - 1) / IF;
		tessera_hash_t *next = malloc(up * sizeof *next,
		    M_TESSERA, M_WAITOK);
		uint64_t *next_max = malloc(up * sizeof *next_max,
		    M_TESSERA, M_WAITOK);
		for (uint32_t i = 0; i < up; i++) {
			uint32_t start = i * IF;
			uint32_t take = leaf_count - start;
			if (take > IF) take = IF;
			rc = tessera_fs_dir_btree_publish_inner(tmp_,
			    layer_max + start, layer + start, take, next[i]);
			if (rc != 0) {
				free(next, M_TESSERA);
				free(next_max, M_TESSERA);
				goto out;
			}
			next_max[i] = layer_max[start + take - 1];
		}
		free(layer, M_TESSERA);
		free(layer_max, M_TESSERA);
		layer = next;
		layer_max = next_max;
		leaf_count = up;
	}
	memcpy(out_root, layer[0], TESSERA_HASH_SIZE);
out:
	free(layer, M_TESSERA);
	free(layer_max, M_TESSERA);
	return (rc);
}

/* Helper for log_checkpoint_parent: collect the parent's current
 * BTREE entries, layered with log overrides. Returns a sorted
 * array (caller frees individual name buffers + arrays). */
struct tessera_dirent_log_collect_ctx {
	uint32_t  cap, count;
	uint64_t *hashes;
	uint64_t *inos;
	char    **names;
	uint16_t *nlens;
};

static int
tessera_dirent_log_collect_cb(void *vctx, uint64_t inode_no,
    const char *name, uint16_t name_len)
{
	struct tessera_dirent_log_collect_ctx *c = vctx;
	if (c->count == c->cap) {
		uint32_t ncap = c->cap == 0 ? 64 : c->cap * 2;
		uint64_t *nh = malloc(ncap * sizeof *nh, M_TESSERA, M_WAITOK);
		uint64_t *ni = malloc(ncap * sizeof *ni, M_TESSERA, M_WAITOK);
		char    **nn = malloc(ncap * sizeof *nn, M_TESSERA, M_WAITOK);
		uint16_t *nl = malloc(ncap * sizeof *nl, M_TESSERA, M_WAITOK);
		if (c->cap > 0) {
			memcpy(nh, c->hashes, c->cap * sizeof *nh);
			memcpy(ni, c->inos,   c->cap * sizeof *ni);
			memcpy(nn, c->names,  c->cap * sizeof *nn);
			memcpy(nl, c->nlens,  c->cap * sizeof *nl);
			free(c->hashes, M_TESSERA); free(c->inos, M_TESSERA);
			free(c->names,  M_TESSERA); free(c->nlens, M_TESSERA);
		}
		c->hashes = nh; c->inos = ni; c->names = nn; c->nlens = nl;
		c->cap = ncap;
	}
	uint32_t i = c->count++;
	c->hashes[i] = tessera_dir_name_hash(name, name_len);
	c->inos[i]   = inode_no;
	char *cp = malloc(name_len, M_TESSERA, M_WAITOK);
	memcpy(cp, name, name_len);
	c->names[i]  = cp;
	c->nlens[i]  = name_len;
	return (0);
}

/* Remove the i-th entry from the collection; caller frees nothing
 * (we shift in place and shrink count). */
static void
tessera_dirent_log_collect_remove_at(
    struct tessera_dirent_log_collect_ctx *c, uint32_t idx)
{
	free(c->names[idx], M_TESSERA);
	uint32_t tail = c->count - idx - 1;
	if (tail > 0) {
		memmove(c->hashes + idx, c->hashes + idx + 1,
		    tail * sizeof *c->hashes);
		memmove(c->inos + idx, c->inos + idx + 1,
		    tail * sizeof *c->inos);
		memmove(c->names + idx, c->names + idx + 1,
		    tail * sizeof *c->names);
		memmove(c->nlens + idx, c->nlens + idx + 1,
		    tail * sizeof *c->nlens);
	}
	c->count--;
}

/* Apply all log entries for `parent_inode_no` to its BTREE in one
 * bulk rebuild. Walks parent's current dir, merges log entries by
 * name (most-recent op wins), bulk-builds a new BTREE, updates
 * parent's manifest_hash. Frees applied log entries.
 *
 * Returns 0 on success, errno otherwise. Caller responsibility:
 * may be called from either a vop hot path (to force-flush a
 * specific parent before readdir) or from tessera_fs_flush (drain
 * all dirty parents before commit_sb). */
static int
tessera_fs_dirent_log_checkpoint_parent(struct tessera_mount *tmp_,
    uint32_t parent_inode_no)
{
	if (!tmp_->dirty_init) return (0);

	/* Snapshot + remove the log entries for this parent. */
	struct tessera_dirent_log_entry **logents = NULL;
	uint32_t logcount = 0, logcap = 32;
	logents = malloc(logcap * sizeof *logents, M_TESSERA, M_WAITOK);

	mtx_lock(&tmp_->flush_mtx);
	uint32_t b = parent_inode_no & (TESSERA_DIRENT_LOG_BUCKETS - 1u);
	struct tessera_dirent_log_entry *e, *next;
	LIST_FOREACH_SAFE(e, &tmp_->dirent_log[b], link, next) {
		if (e->parent_inode_no != parent_inode_no) continue;
		if (logcount == logcap) {
			uint32_t nc = logcap * 2;
			struct tessera_dirent_log_entry **nx =
			    malloc(nc * sizeof *nx, M_TESSERA, M_WAITOK);
			memcpy(nx, logents, logcount * sizeof *logents);
			free(logents, M_TESSERA);
			logents = nx;
			logcap = nc;
		}
		logents[logcount++] = e;
		LIST_REMOVE(e, link);
	}
	tmp_->dirent_log_count -= logcount;
	mtx_unlock(&tmp_->flush_mtx);

	if (logcount == 0) {
		free(logents, M_TESSERA);
		return (0);
	}

	/* Sort by seq (oldest → newest). Insertion sort; logcount is
	 * bounded by dirent_log_threshold. */
	for (uint32_t i = 1; i < logcount; i++) {
		struct tessera_dirent_log_entry *cur = logents[i];
		int32_t j = (int32_t)i - 1;
		while (j >= 0 && logents[j]->seq > cur->seq) {
			logents[j+1] = logents[j];
			j--;
		}
		logents[j+1] = cur;
	}

	/* Read parent inode, fetch current dir contents into a working
	 * collection. */
	uint8_t pkey[4];
	tessera_inode_record_t pino;
	encode_inode_key(parent_inode_no, pkey);
	int rc = 0;
	if (tessera_fs_inode_get_byk(tmp_, pkey, &pino) != TESSERA_OK) {
		rc = EIO;
		goto out_free;
	}

	struct tessera_dirent_log_collect_ctx col = { 0 };
	if (!tessera_hash_is_null(pino.manifest_hash)) {
		rc = tessera_fs_dir_walk(tmp_, pino.manifest_hash,
		    tessera_dirent_log_collect_cb, &col);
		if (rc != 0 && rc != ENOTDIR) goto out_free_col;
		rc = 0;
	}

	/* Apply log entries in seq order to the collection. */
	for (uint32_t i = 0; i < logcount; i++) {
		struct tessera_dirent_log_entry *en = logents[i];
		uint32_t found_idx = (uint32_t)-1;
		for (uint32_t k = 0; k < col.count; k++) {
			if (col.hashes[k] == en->name_hash &&
			    col.nlens[k]  == en->name_len &&
			    memcmp(col.names[k], en->name,
			        en->name_len) == 0) {
				found_idx = k;
				break;
			}
		}
		if (en->op == 0) { /* ADD */
			if (found_idx != (uint32_t)-1) {
				col.inos[found_idx] = en->inode_no;
				continue;
			}
			/* Append. */
			if (col.count == col.cap) {
				uint32_t nc = col.cap == 0 ? 64 : col.cap * 2;
				uint64_t *nh = malloc(nc * sizeof *nh,
				    M_TESSERA, M_WAITOK);
				uint64_t *ni = malloc(nc * sizeof *ni,
				    M_TESSERA, M_WAITOK);
				char    **nn = malloc(nc * sizeof *nn,
				    M_TESSERA, M_WAITOK);
				uint16_t *nl = malloc(nc * sizeof *nl,
				    M_TESSERA, M_WAITOK);
				if (col.cap > 0) {
					memcpy(nh, col.hashes,
					    col.cap * sizeof *nh);
					memcpy(ni, col.inos,
					    col.cap * sizeof *ni);
					memcpy(nn, col.names,
					    col.cap * sizeof *nn);
					memcpy(nl, col.nlens,
					    col.cap * sizeof *nl);
					free(col.hashes, M_TESSERA);
					free(col.inos,   M_TESSERA);
					free(col.names,  M_TESSERA);
					free(col.nlens,  M_TESSERA);
				}
				col.hashes = nh; col.inos = ni;
				col.names  = nn; col.nlens = nl;
				col.cap = nc;
			}
			uint32_t j = col.count++;
			col.hashes[j] = en->name_hash;
			col.inos[j]   = en->inode_no;
			char *cp = malloc(en->name_len, M_TESSERA, M_WAITOK);
			memcpy(cp, en->name, en->name_len);
			col.names[j]  = cp;
			col.nlens[j]  = en->name_len;
		} else { /* REMOVE */
			if (found_idx != (uint32_t)-1)
				tessera_dirent_log_collect_remove_at(&col,
				    found_idx);
		}
	}

	/* Sort collection by name_hash ascending. */
	for (uint32_t i = 1; i < col.count; i++) {
		uint64_t h = col.hashes[i];
		uint64_t in = col.inos[i];
		char *nm = col.names[i];
		uint16_t nl = col.nlens[i];
		int32_t j = (int32_t)i - 1;
		while (j >= 0 && col.hashes[j] > h) {
			col.hashes[j+1] = col.hashes[j];
			col.inos[j+1]   = col.inos[j];
			col.names[j+1]  = col.names[j];
			col.nlens[j+1]  = col.nlens[j];
			j--;
		}
		col.hashes[j+1] = h; col.inos[j+1] = in;
		col.names[j+1]  = nm; col.nlens[j+1] = nl;
	}

	/* Bulk-build a fresh BTREE from the merged collection. */
	tessera_hash_t new_root;
	rc = tessera_fs_dir_btree_bulk_build(tmp_, col.hashes, col.inos,
	    (const char *const *)col.names, col.nlens, col.count,
	    new_root);
	if (rc != 0) goto out_free_col;

	/* Update the parent inode's manifest_hash. */
	memcpy(pino.manifest_hash, new_root, TESSERA_HASH_SIZE);
	pino.gen++;
	{
		struct timeval tv;
		getmicrotime(&tv);
		uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
		    (uint64_t)tv.tv_usec * 1000ULL;
		pino.mtime_ns = now_ns;
		pino.ctime_ns = now_ns;
	}
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_fs_inode_put_byk(tmp_, pkey, &pino,
	    &new_inode_root) != TESSERA_OK) {
		rc = EIO;
		goto out_free_col;
	}
	tmp_->sb.inode_root = new_inode_root;

out_free_col:
	for (uint32_t i = 0; i < col.count; i++)
		free(col.names[i], M_TESSERA);
	free(col.hashes, M_TESSERA); free(col.inos, M_TESSERA);
	free(col.names,  M_TESSERA); free(col.nlens, M_TESSERA);
out_free:
	for (uint32_t i = 0; i < logcount; i++)
		free(logents[i], M_TESSERA);
	free(logents, M_TESSERA);
	return (rc);
}

/* Walk every bucket; checkpoint each unique parent_inode_no found.
 * Used by tessera_fs_flush before commit_sb. */
static int
tessera_fs_dirent_log_checkpoint_all(struct tessera_mount *tmp_)
{
	if (!tmp_->dirty_init) return (0);

	/* Collect unique parent inode_nos under flush_mtx. */
	uint32_t cap = 32, count = 0;
	uint32_t *parents = malloc(cap * sizeof *parents, M_TESSERA, M_WAITOK);

	mtx_lock(&tmp_->flush_mtx);
	for (uint32_t b = 0; b < TESSERA_DIRENT_LOG_BUCKETS; b++) {
		struct tessera_dirent_log_entry *e;
		LIST_FOREACH(e, &tmp_->dirent_log[b], link) {
			int already = 0;
			for (uint32_t i = 0; i < count; i++) {
				if (parents[i] == e->parent_inode_no) {
					already = 1; break;
				}
			}
			if (already) continue;
			if (count == cap) {
				uint32_t nc = cap * 2;
				uint32_t *np = malloc(nc * sizeof *np,
				    M_TESSERA, M_WAITOK);
				memcpy(np, parents, count * sizeof *np);
				free(parents, M_TESSERA);
				parents = np;
				cap = nc;
			}
			parents[count++] = e->parent_inode_no;
		}
	}
	mtx_unlock(&tmp_->flush_mtx);

	int rc = 0;
	for (uint32_t i = 0; i < count; i++) {
		int prc = tessera_fs_dirent_log_checkpoint_parent(tmp_,
		    parents[i]);
		if (prc != 0 && rc == 0) rc = prc;
	}
	free(parents, M_TESSERA);
	return (rc);
}

/* ── v2.6 Phase B.2: journal-resident dirent records ───────────── */

static int tessera_journal_log_enable_default = 1;
SYSCTL_INT(_kern_tessera, OID_AUTO, journal_log_enable,
    CTLFLAG_RW, &tessera_journal_log_enable_default, 0,
    "1 = journal each dirent log entry (group-committed) for crash recovery without explicit fsync");

static int tessera_journal_log_interval_ms = 50;
SYSCTL_INT(_kern_tessera, OID_AUTO, journal_log_interval_ms,
    CTLFLAG_RW, &tessera_journal_log_interval_ms, 0,
    "Group-commit interval for the dirent journal log (ms)");

static unsigned long tessera_stat_journal_log_records = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, journal_log_records,
    CTLFLAG_RD, &tessera_stat_journal_log_records, 0,
    "Cumulative dirent log records appended to the journal");

/* Definition of the replay counter (forward-declared above so the
 * mountfs replay diagnostic can read it). */
SYSCTL_ULONG(_kern_tessera, OID_AUTO, journal_log_replays,
    CTLFLAG_RD, &tessera_stat_journal_log_replays, 0,
    "Cumulative dirent log records re-applied during mount-time replay");

/* B.2 debug — live in-memory journal head/tail of the *most-recently-
 * touched* mount. Updated on every callout drain. Lets a userspace
 * test print these *just before* simulating a crash, then compare
 * against the on-disk header to find divergence. */
static unsigned long tessera_stat_journal_head = 0;
static unsigned long tessera_stat_journal_tail = 0;
SYSCTL_ULONG(_kern_tessera, OID_AUTO, journal_head,
    CTLFLAG_RD, &tessera_stat_journal_head, 0,
    "Last-observed in-memory journal head_block (debug)");
SYSCTL_ULONG(_kern_tessera, OID_AUTO, journal_tail,
    CTLFLAG_RD, &tessera_stat_journal_tail, 0,
    "Last-observed in-memory journal tail_block (debug)");

/* Allocate + clone an entry for the journal-pending queue. Same
 * shape as the in-memory log entry; we duplicate so the per-parent
 * read-side log and the journal-pending list have independent
 * lifetimes (checkpoint frees the read-side; group commit frees
 * the journal-pending). */
static struct tessera_dirent_log_entry *
tessera_dirent_log_entry_clone(
    const struct tessera_dirent_log_entry *src)
{
	size_t sz = sizeof(*src) + src->name_len;
	struct tessera_dirent_log_entry *e = malloc(sz, M_TESSERA, M_WAITOK);
	memcpy(e, src, sizeof(*src));
	memcpy(e->name, src->name, src->name_len);
	return (e);
}

/* Group-commit drain. Snapshot pending records under flush_mtx,
 * release the lock, then do the journal IO without holding it
 * (write_record can sleep / kbio_write). On success: free the
 * snapshot. On failure (journal full, etc.): re-queue at head so
 * the next callout retries. */
static int
tessera_fs_journal_log_drain(struct tessera_mount *tmp_)
{
	if (!tmp_->dirty_init || tmp_->journal == NULL ||
	    tmp_->flush_unmounting)
		return (0);

	/* Snapshot: move pending into a local list head under flush_mtx,
	 * then release before doing the (potentially slow) journal IO. */
	LIST_HEAD(, tessera_dirent_log_entry) snap;
	LIST_INIT(&snap);
	LIST_HEAD(, tessera_pending_inode) isnap;
	LIST_INIT(&isnap);
	uint32_t count = 0;
	uint32_t icount = 0;
	mtx_lock(&tmp_->flush_mtx);
	while (!LIST_EMPTY(&tmp_->journal_pending)) {
		struct tessera_dirent_log_entry *e =
		    LIST_FIRST(&tmp_->journal_pending);
		LIST_REMOVE(e, link);
		LIST_INSERT_HEAD(&snap, e, link);
		count++;
	}
	tmp_->journal_pending_count = 0;
	while (!LIST_EMPTY(&tmp_->journal_pending_inodes)) {
		struct tessera_pending_inode *p =
		    LIST_FIRST(&tmp_->journal_pending_inodes);
		LIST_REMOVE(p, link);
		LIST_INSERT_HEAD(&isnap, p, link);
		icount++;
	}
	tmp_->journal_pending_inode_count = 0;
	mtx_unlock(&tmp_->flush_mtx);

	if (count == 0 && icount == 0) return (0);

	/* Open a tx, append every record, commit. INODE_WRITE records
	 * go in first so a replay applying them in walk order populates
	 * dirty_inodes before the dirent records that reference them. */
	uint64_t tx;
	if (tessera_journal_tx_begin(tmp_->journal, &tx,
	    "log_drain") != TESSERA_OK) {
		/* Re-queue. */
		mtx_lock(&tmp_->flush_mtx);
		while (!LIST_EMPTY(&snap)) {
			struct tessera_dirent_log_entry *e =
			    LIST_FIRST(&snap);
			LIST_REMOVE(e, link);
			LIST_INSERT_HEAD(&tmp_->journal_pending, e, link);
			tmp_->journal_pending_count++;
		}
		while (!LIST_EMPTY(&isnap)) {
			struct tessera_pending_inode *p =
			    LIST_FIRST(&isnap);
			LIST_REMOVE(p, link);
			LIST_INSERT_HEAD(&tmp_->journal_pending_inodes, p, link);
			tmp_->journal_pending_inode_count++;
		}
		mtx_unlock(&tmp_->flush_mtx);
		return (EIO);
	}

	int rc = 0;

	/* Inodes first. */
	while (!LIST_EMPTY(&isnap)) {
		struct tessera_pending_inode *p = LIST_FIRST(&isnap);
		LIST_REMOVE(p, link);

		uint8_t body[sizeof(tessera_jrec_inode_t) +
		    sizeof(tessera_inode_record_t)];
		tessera_jrec_inode_t *ih = (tessera_jrec_inode_t *)body;
		ih->inode_no  = p->inode_no;
		ih->tombstone = (uint8_t)(p->tombstone ? 1 : 0);
		ih->reserved[0] = ih->reserved[1] = ih->reserved[2] = 0;
		memcpy(body + sizeof *ih, &p->rec, sizeof p->rec);

		int ar = tessera_journal_append(tmp_->journal, tx,
		    TESSERA_INODE_WRITE, body, (uint32_t)sizeof body);
		free(p, M_TESSERA);
		if (ar != TESSERA_OK && rc == 0) rc = EIO;
		tessera_stat_journal_log_records++;
	}

	/* Then dirents. */
	while (!LIST_EMPTY(&snap)) {
		struct tessera_dirent_log_entry *e = LIST_FIRST(&snap);
		LIST_REMOVE(e, link);

		size_t blen = sizeof(tessera_jrec_dirent_t) + e->name_len;
		uint8_t *body = malloc(blen, M_TESSERA, M_WAITOK);
		tessera_jrec_dirent_t *hdr = (tessera_jrec_dirent_t *)body;
		hdr->parent_inode_no = e->parent_inode_no;
		hdr->inode_no        = e->inode_no;
		hdr->name_len        = e->name_len;
		hdr->reserved[0] = hdr->reserved[1] = 0;
		memcpy(body + sizeof *hdr, e->name, e->name_len);

		tessera_record_type_t rt = (e->op == 0) ?
		    TESSERA_DIR_INSERT : TESSERA_DIR_REMOVE;
		int ar = tessera_journal_append(tmp_->journal, tx, rt,
		    body, (uint32_t)blen);
		free(body, M_TESSERA);
		free(e, M_TESSERA);
		if (ar != TESSERA_OK && rc == 0) rc = EIO;
		tessera_stat_journal_log_records++;
	}

	if (tessera_journal_tx_commit(tmp_->journal, tx) != TESSERA_OK
	    && rc == 0)
		rc = EIO;

	/* Publish the in-memory journal head/tail so userspace tests
	 * can compare against on-disk state. */
	{
		uint64_t h = 0, t = 0;
		tessera_journal_peek_pos(
		    (const struct tessera_journal *)tmp_->journal, &h, &t);
		tessera_stat_journal_head = (unsigned long)h;
		tessera_stat_journal_tail = (unsigned long)t;
	}
	return (rc);
}

/* Taskqueue handler — runs the actual drain in a sleep-able
 * context. Called via taskqueue_enqueue from the callout below. */
static void
tessera_fs_journal_log_task(void *ctx, int pending)
{
	(void)pending;
	struct tessera_mount *tmp_ = ctx;
	if (tmp_ == NULL || tmp_->flush_unmounting) return;
	(void)tessera_fs_journal_log_drain(tmp_);
}

/* Callout handler — fires every journal_log_interval_ms. Callout
 * runs in interrupt context where sleeping is prohibited, so we
 * just enqueue the actual drain on the kernel taskqueue and re-arm
 * ourselves. The drain does malloc(M_WAITOK) + journal IO which
 * both sleep. */
static void
tessera_fs_journal_log_callout(void *ctx)
{
	struct tessera_mount *tmp_ = ctx;
	if (tmp_ == NULL || tmp_->flush_unmounting) return;
	(void)taskqueue_enqueue(taskqueue_thread, &tmp_->journal_log_task);
	int t_ms = tessera_journal_log_interval_ms;
	if (t_ms < 10) t_ms = 10;
	if (tmp_->journal_log_co_init && !tmp_->flush_unmounting)
		callout_reset(&tmp_->journal_log_co,
		    (hz * t_ms) / 1000,
		    tessera_fs_journal_log_callout, tmp_);
}

/* Replay handler shim — extends the existing replay handler to
 * recognise DIR_INSERT / DIR_REMOVE records and rebuild the
 * in-memory log. Called from tessera_replay_handler. Returns 1 if
 * the record was a dirent op, 0 if not (caller continues with
 * other record types). */
static int
tessera_replay_dirent_record(struct tessera_mount *tmp_,
                             const tessera_record_header_t *hdr,
                             const uint8_t *body)
{
	/* INODE_WRITE — restore dirty_inodes (or tombstone). The
	 * subsequent first-flush drains it to the inode_tree as part
	 * of the same recovery pass that processes DIR records. */
	if (hdr->record_type == (uint32_t)TESSERA_INODE_WRITE) {
		if (hdr->body_length < sizeof(tessera_jrec_inode_t) +
		    sizeof(tessera_inode_record_t))
			return (1);
		tessera_jrec_inode_t ih;
		memcpy(&ih, body, sizeof ih);
		if (ih.tombstone) {
			(void)tessera_fs_inode_delete(tmp_, ih.inode_no);
		} else {
			tessera_inode_record_t rec;
			memcpy(&rec, body + sizeof ih, sizeof rec);
			(void)tessera_fs_inode_put(tmp_, ih.inode_no, &rec);
		}
		tessera_stat_journal_log_replays++;
		return (1);
	}
	if (hdr->record_type != (uint32_t)TESSERA_DIR_INSERT &&
	    hdr->record_type != (uint32_t)TESSERA_DIR_REMOVE)
		return (0);
	if (hdr->body_length < sizeof(tessera_jrec_dirent_t))
		return (1);
	tessera_jrec_dirent_t r;
	memcpy(&r, body, sizeof r);
	if (r.name_len == 0 || r.name_len > TESSERA_PATH_NAME_MAX)
		return (1);
	if (sizeof(tessera_jrec_dirent_t) + r.name_len > hdr->body_length)
		return (1);
	const char *name = (const char *)(body + sizeof r);
	int op = (hdr->record_type == TESSERA_DIR_INSERT) ? 0 : 1;
	(void)tessera_fs_dirent_log_append(tmp_, r.parent_inode_no, op,
	    name, r.name_len, r.inode_no);
	tessera_stat_journal_log_replays++;
	return (1);
}

static int
tessera_fs_dirent_rewrite(struct tessera_mount *tmp_,
                          uint32_t parent_inode_no,
                          int op, uint64_t verify_inode,
                          uint64_t add_inode,
                          const char *name, size_t namelen)
{
	/* v2.6 fast path: route through the dirent log instead of
	 * mutating the BTREE inline. Each call appends a record;
	 * checkpoint (driven from tessera_fs_flush + vop_readdir +
	 * cap-trigger) bulk-applies a batch of pending ops in one
	 * BTREE rebuild per parent. Verify_inode for REMOVE is not
	 * checked at append time — checkpoint enforces it during the
	 * merge. (Mismatched verify_inode would currently silently
	 * still skip the entry in the merge; consistency is held
	 * because in-flight mutations all serialise through the
	 * vfs lookup → caller chain that produced verify_inode in
	 * the first place.) */
	if (tessera_dirent_log_enable_default && tmp_->dirty_init) {
		uint64_t ino = (op == 0) ? add_inode : verify_inode;
		return (tessera_fs_dirent_log_append(tmp_, parent_inode_no,
		    op, name, (uint16_t)namelen, ino));
	}

	int err = 0;
	uint8_t *new_mft = NULL;

	uint8_t pkey[4];
	tessera_inode_record_t pino;
	encode_inode_key(parent_inode_no, pkey);
	if (tessera_fs_inode_get_byk(tmp_, pkey, &pino) != TESSERA_OK)
		return (EIO);

	/* Detect parent's manifest kind. BTREE → dispatch to the
	 * O(log N) helpers. 2L / flat → migrate to BTREE on the fly,
	 * then dispatch. v2.5: BTREE is the canonical dir representation
	 * for any parent that's been mutated post-this-version; old
	 * volumes' 2L / flat dirs read fine but get rewritten to BTREE
	 * on first mutation. */
	{
		uint8_t *peek = NULL;
		uint32_t peek_len = 0;
		tessera_manifest_kind_t pk = TESSERA_MFT_INLINE;
		if (tessera_fs_fetch_blob(tmp_, pino.manifest_hash,
		    &peek, &peek_len) == 0) {
			if (peek_len >= 32) {
				tessera_manifest_parser_t *pp =
				    tessera_manifest_parse(peek, peek_len);
				if (pp != NULL) {
					pk = tessera_manifest_parser_kind(pp);
					tessera_manifest_parser_free(pp);
				}
			}
			free(peek, M_TESSERA);
		}

		tessera_hash_t btree_root;
		int have_root = 0;
		if (pk == TESSERA_MFT_DIRECTORY_BTREE) {
			memcpy(btree_root, pino.manifest_hash,
			    TESSERA_HASH_SIZE);
			have_root = 1;
		} else if (pk == TESSERA_MFT_DIRECTORY ||
		           pk == TESSERA_MFT_DIRECTORY_2L) {
			/* Migrate flat / 2L → BTREE. Walk all entries,
			 * rebuild as a balanced tree. One-time cost on
			 * the first mutation post-upgrade. */
			int empty = 0;
			tessera_hash_t new_root;
			int rc = tessera_fs_dir_btree_migrate(tmp_,
			    pino.manifest_hash, new_root, &empty);
			if (rc != 0) return (rc);
			if (!empty) {
				memcpy(btree_root, new_root,
				    TESSERA_HASH_SIZE);
				have_root = 1;
			}
			/* If empty, fall through with have_root=0 — the
			 * insert path handles "first entry into empty
			 * dir" specially. */
		}

		tessera_hash_t new_root;
		int rc;
		if (op == 0) {
			/* ADD */
			rc = tessera_fs_dir_btree_insert(tmp_,
			    have_root ? btree_root :
			        (const uint8_t *)pino.manifest_hash,
			    /*root_is_empty=*/!have_root, name, namelen,
			    add_inode, new_root);
			if (rc != 0) return (rc);
			memcpy(pino.manifest_hash, new_root,
			    TESSERA_HASH_SIZE);
			goto commit;
		}
		/* REMOVE */
		if (!have_root) return (ENOENT);
		int dropped = 0;
		rc = tessera_fs_dir_btree_remove(tmp_, btree_root, name,
		    namelen, verify_inode, &dropped, new_root);
		if (rc != 0) return (rc);
		if (dropped) {
			/* Last entry gone — the dir is now empty. Still
			 * needs a valid manifest_hash; build an empty
			 * leaf. */
			tessera_manifest_builder_t *mb =
			    tessera_manifest_begin(
			        TESSERA_MFT_DIRECTORY_BTREE);
			if (mb == NULL) return (ENOMEM);
			tessera_manifest_dir_btree_set_leaf(mb, 1);
			size_t mlen = 0;
			tessera_hash_t mh;
			(void)tessera_manifest_finalize(mb, NULL, 0,
			    &mlen, mh);
			uint8_t *buf = malloc(mlen, M_TESSERA, M_WAITOK);
			(void)tessera_manifest_finalize(mb, buf, mlen,
			    &mlen, mh);
			tessera_manifest_free(mb);
			tessera_hash_t pub;
			int prc = tessera_fs_publish_manifest(tmp_, buf,
			    mlen, pub);
			free(buf, M_TESSERA);
			if (prc != 0) return (EIO);
			memcpy(pino.manifest_hash, pub, TESSERA_HASH_SIZE);
		} else {
			memcpy(pino.manifest_hash, new_root,
			    TESSERA_HASH_SIZE);
		}
		goto commit;
	}

	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
	if (mb == NULL) return (ENOMEM);

	struct dirent_rewrite_ctx ctx = {
		.mb            = mb,
		.op            = op,
		.verify_inode  = verify_inode,
		.skip_name     = name,
		.skip_namelen  = namelen,
		.matched       = 0,
		.err           = 0,
	};

	/* Walk handles flat DIRECTORY and DIRECTORY_2L transparently. */
	int rc = tessera_fs_dir_walk(tmp_, pino.manifest_hash,
	    dirent_rewrite_visit, &ctx);
	if (rc != 0) {
		err = (ctx.err != 0) ? ctx.err : rc;
		tessera_manifest_free(mb);
		return (err);
	}
	if (op == 1 && !ctx.matched) {
		tessera_manifest_free(mb);
		return (ENOENT);
	}
	if (op == 0) {
		if (tessera_manifest_add_dirent(mb, add_inode, name,
		    namelen) != TESSERA_OK) {
			tessera_manifest_free(mb);
			return (ENOMEM);
		}
	}

	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	new_mft = malloc(mlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, new_mft, mlen, &mlen,
	    mhash) != TESSERA_OK) {
		tessera_manifest_free(mb);
		free(new_mft, M_TESSERA);
		return (EIO);
	}
	tessera_manifest_free(mb);

	/* Auto-promotes to DIRECTORY_2L if mlen exceeds the threshold.
	 * parent_inode_no tags the cached entry so subsequent dirent
	 * mutations on the same parent supersede the old bytes. */
	tessera_hash_t pub_hash;
	if (tessera_fs_publish_directory(tmp_, parent_inode_no,
	    new_mft, mlen, pub_hash) != 0) {
		free(new_mft, M_TESSERA);
		return (EIO);
	}
	free(new_mft, M_TESSERA);

	memcpy(pino.manifest_hash, pub_hash, sizeof pub_hash);

commit:
	pino.gen++;
	/* POSIX: parent dir's mtime + ctime are updated when its set of
	 * entries changes (any add/remove/rename). pjdfstest's mkdir/00,
	 * symlink/00 etc. assert mtime/ctime advance after the
	 * dir-modifying op. */
	{
		struct timeval tv;
		getmicrotime(&tv);
		uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
		    (uint64_t)tv.tv_usec * 1000ULL;
		pino.mtime_ns = now_ns;
		pino.ctime_ns = now_ns;
	}
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_fs_inode_put_byk(tmp_, pkey, &pino,
	    &new_inode_root) != TESSERA_OK) return (EIO);
	tmp_->sb.inode_root = new_inode_root;
	(void)err;
	return (0);
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
	{
		int aerr = VOP_ACCESS(dvp, VWRITE, cnp->cn_cred, curthread);
		if (aerr != 0) return (aerr);
	}

	int err = 0;
	uint8_t  *child_mft = NULL;

	/* 1. Allocate inode_no. */
	uint32_t new_ino = tessera_fs_alloc_inode_no(tmp_);

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
		{
			uint32_t ifmt;
			switch (vap->va_type) {
			case VSOCK: ifmt = 0140000; break;
			case VFIFO: ifmt = 0010000; break;
			default:    ifmt = 0100000; break;  /* S_IFREG */
			}
			cino.mode = (vap->va_mode & 07777) | ifmt;
		}
		/* va_uid/va_gid normally come in as VNOVAL — the FS is
		 * responsible for setting the new file's owner to the
		 * calling process. UFS/FFS use cnp->cn_cred. */
		cino.uid = (vap->va_uid != (uid_t)VNOVAL)
		    ? vap->va_uid : cnp->cn_cred->cr_uid;
		cino.gid = (vap->va_gid != (gid_t)VNOVAL)
		    ? vap->va_gid : cnp->cn_cred->cr_groups[0];
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
		if (tessera_fs_inode_put_byk(tmp_, ckey, &cino,
		    &new_inode_root) != TESSERA_OK) { err = EIO; goto out; }
		tmp_->sb.inode_root = new_inode_root;
	}

	/* 4. Rewrite parent DIRECTORY manifest with the new entry.
	 * dirent_rewrite handles flat + DIRECTORY_2L parents and auto-
	 * promotes the published manifest if it crosses the threshold.
	 * It also btree_puts the updated parent inode internally, so
	 * tmp_->sb.inode_root is already advanced on return. */
	{
		int drc = tessera_fs_dirent_rewrite(tmp_,
		    (uint32_t)dn->inode_no,
		    /*op=ADD*/ 0, /*verify*/ 0,
		    /*add_inode*/ new_ino,
		    cnp->cn_nameptr, cnp->cn_namelen);
		if (drc != 0) { err = drc; goto out; }
	}

	/* 5. Get a deduped vnode for the new inode. tessera_vget reads
	 * the just-written inode record, sets v_type=VREG. */
	struct vnode *cvp;
	if (tessera_vget(dvp->v_mount, new_ino, dn->inode_no, &cvp) != 0) {
		err = EIO; goto out;
	}
	*vpp = cvp;

	/* 6. Commit. */
	/* v2-step-2a: SB write + commit_extent are both deferred to
	 * tessera_fs_flush. extent_flush_via builds a fresh tree from
	 * the in-memory state on each call without freeing the old
	 * one's sectors — calling it per-vop leaks meta-reserve at
	 * ~3 sectors per call (bug #2). */
	tessera_fs_mark_dirty(tmp_);

out:
	if (child_mft) free(child_mft, M_TESSERA);
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
	if (tessera_fs_inode_get_byk(tmp_, key, &ino) != TESSERA_OK)
		return (EIO);

	if (ioflag & IO_APPEND)
		uio->uio_offset = (off_t)ino.size;

	const uint64_t write_off  = (uint64_t)uio->uio_offset;
	const uint64_t write_resid = (uint64_t)uio->uio_resid;
	const uint64_t write_end  = write_off + write_resid;
	const uint64_t final_size = write_end > ino.size ? write_end : ino.size;
	if (final_size > TESSERA_WRITE_MAX_BYTES)
		return (EFBIG);

	/* Quota: reserve the logical growth before doing the write, so the
	 * write is all-or-nothing against the limit (tessera-quotas.md §3.3).
	 * A pure overwrite (final_size == ino.size) costs nothing. Reservation
	 * happens once here, ahead of both the buffered and slow paths.
	 * Under the vnode lock, so no separate domain lock is needed yet. */
	if (tmp_->quota_active && final_size > ino.size) {
		uint64_t delta = final_size - ino.size;
		if (tessera_quota_reserve(&tmp_->quota_dom, delta) != TESSERA_OK)
			return (EDQUOT);
	}

	/* Coalesce small/medium writes into a per-inode RAM buffer so
	 * the manifest is published once at fsync / flush, not once per
	 * vop_write. Files up to tessera_dirty_content_file_max
	 * (default 4 MiB) qualify; the buffer publishes through INLINE
	 * or chunked depending on its size at flush time.
	 *
	 * Hot path: copy directly from user into the dirty_content
	 * buffer — no kbuf malloc, no extra memcpy. The vnode lock held
	 * by VFS guarantees no concurrent drain. */
	if (final_size <= (uint64_t)tessera_dirty_content_file_max) {
		int wr = tessera_fs_dirty_content_write_uio(tmp_,
		    (uint32_t)tn->inode_no, write_off, uio,
		    (size_t)final_size);
		if (wr != 0) return (wr);
		/* Size update is implicit: inode_get overlays the live
		 * size from dirty_content, so no inode_put needed for
		 * size. Setuid strip is the only mode change that
		 * requires an inode record update. */
		if ((ino.mode & 06000) != 0 && ap->a_cred != NULL &&
		    priv_check_cred(ap->a_cred,
		        PRIV_VFS_RETAINSUGID) != 0) {
			ino.mode &= ~06000;
			(void)tessera_fs_inode_put(tmp_,
			    (uint32_t)tn->inode_no, &ino);
		}
		vnode_pager_setsize(vp, final_size);
		/* Note: vop_write_inline / vop_write_chunked counters are
		 * incremented at publish time (in dirty_content_publish or
		 * the slow path below), not here — buffered writes don't
		 * yet know which manifest kind they'll publish as. */
		tessera_fs_mark_dirty(tmp_);
		return (0);
	}

	/* Slow paths beyond this point materialise new_bytes from the
	 * uio first; they need the bytes in kernel memory anyway for
	 * read_full_content / hash. */
	uint8_t *new_bytes = malloc((size_t)write_resid, M_TESSERA, M_WAITOK);
	int err = uiomove(new_bytes, (int)write_resid, uio);
	if (err != 0) {
		free(new_bytes, M_TESSERA);
		return (err);
	}

	/* If we have a coalesced buffer but the write spills past
	 * dirty_content_file_max, drain it to disk first so the chunked
	 * path below sees the latest content (and so we stop holding the
	 * old size's bytes in RAM). */
	(void)tessera_fs_dirty_content_drain_one(tmp_, (uint32_t)tn->inode_no);
	/* Re-fetch ino; the drain may have grown it. */
	if (tessera_fs_inode_get_byk(tmp_, key, &ino) != TESSERA_OK) {
		free(new_bytes, M_TESSERA);
		return (EIO);
	}

	/* Append fast-path (step-3b): pure append into a chunked file
	 * skips materialising the existing bytes entirely. Eligibility
	 * checks live in tessera_fs_append_chunked; on ENOTSUP we fall
	 * through to the slow read-modify-write path below. */
	if (write_off == ino.size &&
	    final_size > TESSERA_INLINE_THRESHOLD) {
		const uint32_t cs = tessera_chunk_size_for(tmp_, final_size);
		int frc = tessera_fs_append_chunked(tmp_,
		    (uint32_t)tn->inode_no, new_bytes,
		    (size_t)write_resid, cs);
		if (frc == 0) {
			tessera_stat_append_fast_ok++;
			tessera_stat_vop_write_chunked++;
			free(new_bytes, M_TESSERA);
			if ((ino.mode & 06000) != 0 && ap->a_cred != NULL &&
			    priv_check_cred(ap->a_cred,
			        PRIV_VFS_RETAINSUGID) != 0) {
				tessera_inode_record_t ino2;
				if (tessera_fs_inode_get(tmp_,
				    (uint32_t)tn->inode_no, &ino2)
				    == TESSERA_OK) {
					ino2.mode &= ~06000;
					(void)tessera_fs_inode_put(tmp_,
					    (uint32_t)tn->inode_no, &ino2);
				}
			}
			vnode_pager_setsize(vp, final_size);
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

	/* POSIX: after a successful write, S_ISUID and S_ISGID are
	 * cleared unless the caller has appropriate privilege. Refetch
	 * the inode (replace_content advanced the cache copy), clear
	 * the bits, and stash the result back. */
	if ((ino.mode & 06000) != 0 && ap->a_cred != NULL &&
	    priv_check_cred(ap->a_cred, PRIV_VFS_RETAINSUGID) != 0) {
		tessera_inode_record_t ino2;
		if (tessera_fs_inode_get(tmp_, (uint32_t)tn->inode_no,
		    &ino2) == TESSERA_OK) {
			ino2.mode &= ~06000;  /* strip S_ISUID | S_ISGID */
			(void)tessera_fs_inode_put(tmp_,
			    (uint32_t)tn->inode_no, &ino2);
		}
	}

	if (final_size != ino.size)
		vnode_pager_setsize(vp, final_size);
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
	{
		int aerr = VOP_ACCESS(dvp, VWRITE, cnp->cn_cred, curthread);
		if (aerr != 0) return (aerr);
	}

	int err = 0;
	uint8_t *child_mft = NULL;

	uint32_t new_ino = tessera_fs_alloc_inode_no(tmp_);

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
	cino.uid      = (vap->va_uid != (uid_t)VNOVAL)
	    ? vap->va_uid : cnp->cn_cred->cr_uid;
	cino.gid      = (vap->va_gid != (gid_t)VNOVAL)
	    ? vap->va_gid : cnp->cn_cred->cr_groups[0];
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
	if (tessera_fs_inode_put_byk(tmp_, ckey, &cino,
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

	/* v2-step-2a: SB write + commit_extent are both deferred to
	 * tessera_fs_flush. extent_flush_via builds a fresh tree from
	 * the in-memory state on each call without freeing the old
	 * one's sectors — calling it per-vop leaks meta-reserve at
	 * ~3 sectors per call (bug #2). */
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
	{
		int aerr = VOP_ACCESS(dvp, VWRITE, cnp->cn_cred, curthread);
		if (aerr != 0) return (aerr);
	}
	{
		int serr = tessera_fs_sticky_check(tmp_, dn, cn, cnp->cn_cred);
		if (serr != 0) return (serr);
	}

	/* v2.6: force-checkpoint the dirent log for the dir we're
	 * about to rmdir, so the BTREE-based empty check below sees the
	 * post-log state. Without this, an empty dir whose only
	 * remaining REMOVE op is still pending in the log looks
	 * non-empty to the manifest walk. */
	(void)tessera_fs_dirent_log_checkpoint_parent(tmp_,
	    (uint32_t)cn->inode_no);

	/* Fetch child inode + manifest, verify it's empty. */
	uint8_t ckey[4];
	tessera_inode_record_t cino;
	encode_inode_key((uint32_t)cn->inode_no, ckey);
	if (tessera_fs_inode_get_byk(tmp_, ckey, &cino) != TESSERA_OK)
		return (EIO);
	if ((cino.mode & 0170000) != 0040000)
		return (ENOTDIR);
	uint8_t *cblob = NULL;
	uint32_t cblob_len = 0;
	if (!tessera_hash_is_null(cino.manifest_hash)) {
		if (tessera_fs_fetch_blob(tmp_, cino.manifest_hash,
		    &cblob, &cblob_len) != 0)
			return (EIO);
		/* Determine "empty" by manifest entry_count, not raw
		 * blob length. v2.5 BTREE root nodes always carry an
		 * 8-byte body header even when empty, so the prior
		 * `cblob_len > 32` heuristic falsely flagged BTREE
		 * empty-dirs as non-empty. */
		int empty = 1;
		if (cblob_len >= 32) {
			tessera_manifest_parser_t *p =
			    tessera_manifest_parse(cblob, cblob_len);
			if (p != NULL) {
				if (tessera_manifest_parser_count(p) > 0)
					empty = 0;
				tessera_manifest_parser_free(p);
			}
		}
		if (!empty) {
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
	if (tessera_fs_inode_delete_byk(tmp_, ckey,
	    &new_inode_root) != TESSERA_OK)
		printf("tessera_fs: vop_rmdir — btree_delete child "
		    "inode=%u failed\n", (unsigned)cn->inode_no);
	else
		tmp_->sb.inode_root = new_inode_root;

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
	{
		int aerr = VOP_ACCESS(dvp, VWRITE, cnp->cn_cred, curthread);
		if (aerr != 0) return (aerr);
	}

	int err = 0;
	uint8_t *child_mft = NULL;

	uint32_t new_ino = tessera_fs_alloc_inode_no(tmp_);

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
	cino.uid      = cnp->cn_cred->cr_uid;
	cino.gid      = cnp->cn_cred->cr_groups[0];
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
	if (tessera_fs_inode_put_byk(tmp_, ckey, &cino,
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

	/* v2-step-2a: SB write + commit_extent are both deferred to
	 * tessera_fs_flush. extent_flush_via builds a fresh tree from
	 * the in-memory state on each call without freeing the old
	 * one's sectors — calling it per-vop leaks meta-reserve at
	 * ~3 sectors per call (bug #2). */
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
	if (tessera_fs_inode_get_byk(tmp_, key, &ino) != TESSERA_OK)
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

	/* POSIX: linking a name into a directory requires write permission
	 * on that directory. */
	{
		int aerr = VOP_ACCESS(tdvp, VWRITE, cnp->cn_cred, curthread);
		if (aerr != 0) return (aerr);
	}

	uint8_t ckey[4];
	tessera_inode_record_t cino;
	encode_inode_key((uint32_t)cn->inode_no, ckey);
	if (tessera_fs_inode_get_byk(tmp_, ckey, &cino) != TESSERA_OK)
		return (EIO);
	cino.nlink++;
	struct timeval tv;
	getmicrotime(&tv);
	cino.ctime_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
	    (uint64_t)tv.tv_usec * 1000ULL;
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_fs_inode_put_byk(tmp_, ckey, &cino,
	    &new_inode_root) != TESSERA_OK)
		return (EIO);
	tmp_->sb.inode_root = new_inode_root;

	int err = tessera_fs_dirent_rewrite(tmp_, (uint32_t)dn->inode_no,
	    /*op*/ 0, /*verify*/ 0, /*add*/ cn->inode_no,
	    cnp->cn_nameptr, cnp->cn_namelen);
	if (err != 0) {
		/* Roll back nlink bump on dirent failure. */
		cino.nlink--;
		(void)tessera_fs_inode_put_byk(tmp_, ckey, &cino,
		    &new_inode_root);
		tmp_->sb.inode_root = new_inode_root;
		return (err);
	}

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
struct rename_ctx {
	tessera_manifest_builder_t *mb;
	uint64_t target_inode;
	const char *old_name; size_t old_namelen;
	const char *new_name; size_t new_namelen;
	int matched;
	int conflict;
	int err;
};

static int
rename_visit(void *vctx, uint64_t ch, const char *nm, uint16_t nl)
{
	struct rename_ctx *c = vctx;
	int is_old = ((size_t)nl == c->old_namelen) &&
	    (memcmp(nm, c->old_name, c->old_namelen) == 0);
	int is_new = ((size_t)nl == c->new_namelen) &&
	    (memcmp(nm, c->new_name, c->new_namelen) == 0);
	if (is_old) {
		c->matched = 1;
		if (ch != c->target_inode) {
			c->err = EIO;
			return (EIO);
		}
		return (0);  /* drop the old entry */
	}
	if (is_new) c->conflict = 1;
	if (tessera_manifest_add_dirent(c->mb, ch, nm, nl)
	    != TESSERA_OK) {
		c->err = ENOMEM;
		return (ENOMEM);
	}
	return (0);
}

static int
tessera_fs_dirent_rename_same_dir(struct tessera_mount *tmp_,
    uint32_t parent_inode_no,
    uint64_t target_inode,
    const char *old_name, size_t old_namelen,
    const char *new_name, size_t new_namelen)
{
	uint8_t pkey[4];
	tessera_inode_record_t pino;
	encode_inode_key(parent_inode_no, pkey);
	if (tessera_fs_inode_get_byk(tmp_, pkey, &pino) != TESSERA_OK)
		return (EIO);

	tessera_manifest_builder_t *mb =
	    tessera_manifest_begin(TESSERA_MFT_DIRECTORY);
	if (mb == NULL) return (ENOMEM);

	struct rename_ctx ctx = {
		.mb = mb,
		.target_inode = target_inode,
		.old_name = old_name, .old_namelen = old_namelen,
		.new_name = new_name, .new_namelen = new_namelen,
		.matched = 0, .conflict = 0, .err = 0,
	};

	int rc = tessera_fs_dir_walk(tmp_, pino.manifest_hash,
	    rename_visit, &ctx);

	if (rc != 0) {
		tessera_manifest_free(mb);
		return (ctx.err != 0 ? ctx.err : rc);
	}
	if (!ctx.matched) {
		tessera_manifest_free(mb);
		return (ENOENT);
	}
	if (ctx.conflict) {
		tessera_manifest_free(mb);
		return (EEXIST);
	}
	if (tessera_manifest_add_dirent(mb, target_inode, new_name,
	    new_namelen) != TESSERA_OK) {
		tessera_manifest_free(mb);
		return (ENOMEM);
	}

	size_t mlen = 0;
	tessera_hash_t mhash;
	(void)tessera_manifest_finalize(mb, NULL, 0, &mlen, mhash);
	uint8_t *new_mft = malloc(mlen, M_TESSERA, M_WAITOK);
	if (tessera_manifest_finalize(mb, new_mft, mlen, &mlen,
	    mhash) != TESSERA_OK) {
		tessera_manifest_free(mb);
		free(new_mft, M_TESSERA);
		return (EIO);
	}
	tessera_manifest_free(mb);

	tessera_hash_t pub_hash;
	if (tessera_fs_publish_directory(tmp_, parent_inode_no,
	    new_mft, mlen, pub_hash) != 0) {
		free(new_mft, M_TESSERA);
		return (EIO);
	}
	free(new_mft, M_TESSERA);

	memcpy(pino.manifest_hash, pub_hash, sizeof pub_hash);
	pino.gen++;
	{
		struct timeval tv;
		getmicrotime(&tv);
		uint64_t now_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
		    (uint64_t)tv.tv_usec * 1000ULL;
		pino.mtime_ns = now_ns;
		pino.ctime_ns = now_ns;
	}
	uint64_t new_inode_root = tmp_->sb.inode_root;
	if (tessera_fs_inode_put_byk(tmp_, pkey, &pino,
	    &new_inode_root) != TESSERA_OK) return (EIO);
	tmp_->sb.inode_root = new_inode_root;
	return (0);
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
	if (tessera_fs_inode_get_byk(tmp_, key, &ino) != TESSERA_OK)
		return (EIO);
	if (ino.nlink > 1) {
		ino.nlink--;
		struct timeval tv;
		getmicrotime(&tv);
		ino.ctime_ns = (uint64_t)tv.tv_sec * 1000000000ULL +
		    (uint64_t)tv.tv_usec * 1000ULL;
		uint64_t new_root = tmp_->sb.inode_root;
		if (tessera_fs_inode_put_byk(tmp_, key, &ino,
		    &new_root) != TESSERA_OK)
			return (EIO);
		tmp_->sb.inode_root = new_root;
		return (0);
	}
	/* Quota: the last name is gone and the inode is being deleted —
	 * release its logical size back to the domain (tessera-quotas.md
	 * §5.3). Use the size-overlay inode_get so any unflushed coalesced
	 * writes are counted, matching what vop_write reserved. */
	if (tmp_->quota_active) {
		tessera_inode_record_t live;
		if (tessera_fs_inode_get(tmp_, inode_no, &live) == TESSERA_OK)
			tessera_quota_release(&tmp_->quota_dom, live.size);
	}
	uint64_t new_root = tmp_->sb.inode_root;
	if (tessera_fs_inode_delete_byk(tmp_, key,
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
	/* Rename needs VWRITE on both parent dirs (source's for the
	 * unlink, target's for the new dirent). UFS does the same. */
	{
		int aerr = VOP_ACCESS(fdvp, VWRITE, fcnp->cn_cred, curthread);
		if (aerr != 0) { err = aerr; goto release; }
		if (tdvp != fdvp) {
			aerr = VOP_ACCESS(tdvp, VWRITE, tcnp->cn_cred,
			    curthread);
			if (aerr != 0) { err = aerr; goto release; }
		}
	}
	struct tessera_mount *tmp_ = VFSTOTESSERA(fdvp->v_mount);
	struct tessera_node  *fdn  = VTOTNODE(fdvp);
	struct tessera_node  *tdn  = VTOTNODE(tdvp);
	struct tessera_node  *fn   = VTOTNODE(fvp);
	struct tessera_node  *tn   = (tvp != NULL) ? VTOTNODE(tvp) : NULL;
	/* Sticky-bit checks (POSIX): in a directory with S_ISVTX, only
	 * root, the dir owner, or the file owner may unlink/rename a
	 * file. Apply on source (we're "removing" fn from fdvp) and on
	 * target (if overwriting, we're effectively unlinking tn from
	 * tdvp). */
	{
		int serr = tessera_fs_sticky_check(tmp_, fdn, fn, fcnp->cn_cred);
		if (serr != 0) { err = serr; goto release; }
		if (tn != NULL) {
			serr = tessera_fs_sticky_check(tmp_, tdn, tn,
			    tcnp->cn_cred);
			if (serr != 0) { err = serr; goto release; }
		}
	}
	/* POSIX: rename of a directory into itself or a subdirectory of
	 * itself must fail with EINVAL. Tessera's on-disk inode record
	 * doesn't store parent_inode_no, so we do the one-level check
	 * that's reachable via the in-memory node: target dir == source,
	 * or target dir's direct parent == source. This catches the
	 * pjdfstest rename/18 case (rename A A/B/C) and any
	 * one-level-deep variant. UFS does an unbounded walk via
	 * ufs_checkpath; deeper cases here would need an in-memory
	 * ancestor chain or an on-disk parent pointer (deferred). */
	if (fvp->v_type == VDIR &&
	    (tdn->inode_no == fn->inode_no ||
	     tdn->parent_inode_no == fn->inode_no)) {
		err = EINVAL;
		goto release;
	}
	if (tvp != NULL) {
		/* POSIX type matching: regular ↔ regular, dir ↔ empty-dir. */
		if ((fvp->v_type == VDIR) != (tvp->v_type == VDIR)) {
			err = ((fvp->v_type == VDIR) ? ENOTDIR : EISDIR);
			goto release;
		}
		if (tvp->v_type == VDIR) {
			/* Target dir must be empty.
			 * v2.6: force-checkpoint the target dir so the
			 * manifest reflects any pending log REMOVEs. */
			(void)tessera_fs_dirent_log_checkpoint_parent(tmp_,
			    (uint32_t)tn->inode_no);
			uint8_t tkey[4];
			tessera_inode_record_t tino;
			encode_inode_key((uint32_t)tn->inode_no, tkey);
			if (tessera_fs_inode_get_byk(tmp_, tkey, &tino)
			    != TESSERA_OK) { err = EIO; goto release; }
			uint8_t *tblob = NULL;
			uint32_t tblob_len = 0;
			if (!tessera_hash_is_null(tino.manifest_hash) &&
			    tessera_fs_fetch_blob(tmp_, tino.manifest_hash,
			    &tblob, &tblob_len) == 0) {
				/* Use manifest entry_count to detect
				 * "empty" (BTREE empty leaf has an
				 * 8-byte body header). */
				int empty = 1;
				if (tblob_len >= 32) {
					tessera_manifest_parser_t *pp =
					    tessera_manifest_parse(tblob,
					        tblob_len);
					if (pp != NULL) {
						if (tessera_manifest_parser_count(pp) > 0)
							empty = 0;
						tessera_manifest_parser_free(pp);
					}
				}
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
		/* Same-dir, no collision. Two dirent_rewrite calls (ADD
		 * then REMOVE) instead of the legacy
		 * tessera_fs_dirent_rename_same_dir slow path — the
		 * legacy helper walked the whole parent manifest for
		 * the rename, which on DIRECTORY_2L parents is O(N).
		 * Going through dirent_rewrite picks up the bucket-
		 * targeted fast path (O(N/16)) and an extra btree_put
		 * is cheap by comparison. */
		err = tessera_fs_dirent_rewrite(tmp_,
		    (uint32_t)fdn->inode_no,
		    /*op=ADD*/ 0, /*verify*/ 0, /*add*/ fn->inode_no,
		    tcnp->cn_nameptr, tcnp->cn_namelen);
		if (err == 0) {
			err = tessera_fs_dirent_rewrite(tmp_,
			    (uint32_t)fdn->inode_no,
			    /*op=REMOVE*/ 1, /*verify*/ fn->inode_no,
			    /*add*/ 0,
			    fcnp->cn_nameptr, fcnp->cn_namelen);
		}
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

	tessera_fs_mark_dirty(tmp_);

release:
	/* fdvp + fvp came UNLOCKED → vrele only */
	if (tvp != NULL) vput(tvp);
	vput(tdvp);
	vrele(fdvp);
	vrele(fvp);
	return (err);
}

/*
 * vop_pathconf — POSIX runtime tunables. pjdfstest's namegen_max
 * relies on _PC_NAME_MAX returning a sane value: without this vop the
 * default returns -1/EINVAL on subdirectories (only the mount root
 * picks up f_namemax via vfs_stdpathconf), and every rename test that
 * builds a max-length name from pathconf produces empty arguments.
 *
 * Mirrors UFS structure but with tessera's own constants. _PC_LINK_MAX
 * uses a generous default (UFS uses 32767 — same here). _PC_PATH_MAX
 * is left to the vfs_stdpathconf default.
 */
static int
tessera_vop_pathconf(struct vop_pathconf_args *ap)
{
	int err = 0;
	switch (ap->a_name) {
	case _PC_LINK_MAX:
		*ap->a_retval = 32767;
		break;
	case _PC_NAME_MAX:
		*ap->a_retval = TESSERA_PATH_NAME_MAX;
		break;
	case _PC_PIPE_BUF:
		if (ap->a_vp->v_type == VDIR || ap->a_vp->v_type == VFIFO)
			*ap->a_retval = PIPE_BUF;
		else
			err = EINVAL;
		break;
	case _PC_CHOWN_RESTRICTED:
		*ap->a_retval = 1;
		break;
	case _PC_NO_TRUNC:
		*ap->a_retval = 1;
		break;
	case _PC_FILESIZEBITS:
		*ap->a_retval = 64;
		break;
	case _PC_MIN_HOLE_SIZE:
		*ap->a_retval = ap->a_vp->v_mount->mnt_stat.f_iosize;
		break;
	case _PC_ALLOC_SIZE_MIN:
		*ap->a_retval = ap->a_vp->v_mount->mnt_stat.f_bsize;
		break;
	case _PC_REC_INCR_XFER_SIZE:
	case _PC_REC_XFER_ALIGN:
		*ap->a_retval = ap->a_vp->v_mount->mnt_stat.f_iosize;
		break;
	case _PC_REC_MAX_XFER_SIZE:
	case _PC_REC_MIN_XFER_SIZE:
		*ap->a_retval = -1;
		break;
	case _PC_SYMLINK_MAX:
		*ap->a_retval = MAXPATHLEN;
		break;
	default:
		err = vop_stdpathconf(ap);
		break;
	}
	return (err);
}

struct vop_vector tessera_vnodeops = {
	.vop_default  = &default_vnodeops,
	.vop_pathconf = tessera_vop_pathconf,
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
	.vop_getpages = tessera_vop_getpages,
	.vop_putpages = tessera_vop_putpages,
	.vop_fsync    = tessera_vop_fsync,
	.vop_reclaim  = tessera_vop_reclaim,
};
VFS_VOP_VECTOR_REGISTER(tessera_vnodeops);

VFS_SET(tessera_vfsops, tessera, 0);
MODULE_VERSION(tessera_fs, 1);
