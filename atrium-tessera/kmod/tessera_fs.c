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
#include <sys/malloc.h>
#include <sys/uio.h>
#include <sys/types.h>
#include <sys/buf.h>
#include <sys/conf.h>
#include <sys/priv.h>
#include <sys/fcntl.h>
#include <geom/geom.h>
#include <geom/geom_vfs.h>

#include "tessera/btree.h"
#include "tessera/codec.h"
#include "tessera/error.h"
#include "tessera/format.h"
#include "tessera/hash.h"
#include "tessera/manifest.h"
#include "tessera/pack.h"
#include "tessera/volume.h"

MALLOC_DEFINE(M_TESSERA, "tessera", "Tessera filesystem");

/* ── kmod block_io shim ──────────────────────────────────────── */

/* tessera-core's primitives talk to "disk" via tessera_block_io_t.
 * In kernel mode we back it with bread/bwrite via the GEOM consumer.
 * For round-3 read-only use the alloc/free callbacks are stubs;
 * round-4 will wire alloc to tessera_extent_alloc against the
 * volume's free-extent tree. */

struct tessera_kbio_ctx {
	struct vnode *devvp;
	struct ucred *cred;
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

static int
tessera_kbio_alloc(void *ctx, uint64_t n, uint64_t *out_sector)
{ (void)ctx; (void)n; (void)out_sector; return (-1); }

static int
tessera_kbio_free(void *ctx, uint64_t s, uint64_t n)
{ (void)ctx; (void)s; (void)n; return (0); }

/* ── forward decl for fetch_blob (uses tessera_mount) ────────── */
struct tessera_mount;
static int tessera_fs_fetch_blob(struct tessera_mount *tmp_,
                                  const tessera_hash_t hash,
                                  uint8_t **out_buf, uint32_t *out_len);

/* ── per-mount state ─────────────────────────────────────────── */

struct tessera_mount {
	struct vnode             *devvp;     /* the block-device vnode */
	struct g_consumer        *cp;        /* GEOM consumer for I/O */
	struct cdev              *dev;
	tessera_superblock_t      sb;
	struct tessera_kbio_ctx   bio_ctx;
	tessera_block_io_t        bio;
	tessera_btree_t          *inode_tree;
	tessera_btree_t          *pack_registry_tree;
};

#define VFSTOTESSERA(mp) ((struct tessera_mount *)((mp)->mnt_data))

/* ── per-vnode private ──────────────────────────────────────── */

struct tessera_node {
	uint64_t inode_no;
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
tessera_mountfs(struct vnode *devvp, struct mount *mp)
{
	struct tessera_mount *tmp_;
	struct g_consumer    *cp = NULL;
	struct cdev          *dev = devvp->v_rdev;
	struct bufobj        *bo;
	tessera_superblock_t  sb_a, sb_b, *active = NULL;
	int err;

	dev_ref(dev);
	/* Open the device read-write at the GEOM layer so we can issue
	 * the SB self-heal write below; user-facing writes are still
	 * blocked by MNT_RDONLY in this round. */
	g_topology_lock();
	err = g_vfs_open(devvp, &cp, "tessera", 1);
	g_topology_unlock();
	VOP_UNLOCK(devvp);
	if (err != 0) {
		dev_rel(dev);
		return (err);
	}

	if (cp->provider->sectorsize > TESSERA_SECTOR_SIZE ||
	    (TESSERA_SECTOR_SIZE % cp->provider->sectorsize) != 0) {
		err = EINVAL;
		goto fail_close;
	}

	bo = &devvp->v_bufobj;

	int valid_a = (tessera_load_sb(devvp, 0, &sb_a) == 0);
	int valid_b = (tessera_load_sb(devvp, 1, &sb_b) == 0);
	if (!valid_a && !valid_b) {
		printf("tessera_fs: neither superblock decoded; refusing to mount\n");
		err = EINVAL;
		goto fail_close;
	}
	if (valid_a && valid_b)
		active = (sb_a.generation >= sb_b.generation) ? &sb_a : &sb_b;
	else
		active = valid_a ? &sb_a : &sb_b;

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
	 * but don't block the mount. */
	if (!valid_a) {
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
	} else if (sb_a.generation != sb_b.generation) {
		uint64_t which = (active == &sb_a) ? 1 : 0;
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

	/* Wire the kmod block_io shim and open the inode tree against it.
	 * If the tree open fails (e.g. corrupted root sector) we still
	 * mount — the synthesized root vnode lets `df` / `umount` work
	 * for diagnostic purposes. */
	tmp_->bio_ctx.devvp = devvp;
	tmp_->bio_ctx.cred  = curthread->td_ucred;
	tmp_->bio.read_block  = tessera_kbio_read;
	tmp_->bio.write_block = tessera_kbio_write;
	tmp_->bio.alloc       = tessera_kbio_alloc;
	tmp_->bio.free        = tessera_kbio_free;
	tmp_->bio.ctx         = &tmp_->bio_ctx;
	tmp_->inode_tree = tessera_btree_open(&tmp_->bio,
	    tmp_->sb.inode_root, /*tree_kind*/ 0,
	    /*key*/ 4, /*value*/ TESSERA_INODE_RECORD_SIZE);
	if (tmp_->inode_tree == NULL)
		printf("tessera_fs: warning — inode tree open at sector %lu "
		    "failed; root will be synthesized\n",
		    (unsigned long)tmp_->sb.inode_root);

	tmp_->pack_registry_tree = tessera_btree_open(&tmp_->bio,
	    tmp_->sb.pack_registry_root, /*tree_kind*/ 1,
	    /*key*/ 16, /*value*/ TESSERA_REGISTRY_ENTRY_SIZE);
	if (tmp_->pack_registry_tree == NULL)
		printf("tessera_fs: warning — pack registry open at sector %lu "
		    "failed; blob lookups will fail\n",
		    (unsigned long)tmp_->sb.pack_registry_root);

	mp->mnt_data = tmp_;
	mp->mnt_stat.f_namemax = TESSERA_PATH_NAME_MAX;
	mp->mnt_flag |= MNT_LOCAL | MNT_RDONLY;
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

	err = tessera_mountfs(devvp, mp);
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
		if (tmp_->pack_registry_tree != NULL)
			tessera_btree_close(tmp_->pack_registry_tree);
		if (tmp_->inode_tree != NULL)
			tessera_btree_close(tmp_->inode_tree);
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

static int
tessera_root_impl(struct mount *mp, int flags, struct vnode **vpp)
{
	struct vnode *vp;
	struct tessera_node *tn;
	int err;

	err = getnewvnode("tessera", mp, &tessera_vnodeops, &vp);
	if (err != 0) return (err);

	(void)vn_lock(vp, LK_EXCLUSIVE | LK_RETRY);

	tn = malloc(sizeof(*tn), M_TESSERA, M_WAITOK | M_ZERO);
	tn->inode_no = TESSERA_INODE_ROOT_DIR;

	vp->v_data  = tn;
	vp->v_type  = VDIR;
	vp->v_vflag |= VV_ROOT;
	VN_LOCK_ASHARE(vp);

	err = insmntque1(vp, mp);
	if (err != 0) {
		vp->v_data = NULL;
		vp->v_op = &dead_vnodeops;
		vgone(vp);
		vput(vp);
		free(tn, M_TESSERA);
		return (err);
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
	sbp->f_bfree  = tmp_->sb.pack_zone_length;
	sbp->f_bavail = tmp_->sb.pack_zone_length;
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
	if (ap->a_accmode & VWRITE) return (EROFS);
	return (0);
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
		vref(dvp);
		*vpp = dvp;
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
	if (rc != 0) return (rc);

	/* Found: build a child vnode. (vop_getattr will read the child's
	 * record on demand when the caller stats the result.) */
	struct vnode *cvp;
	int e = getnewvnode("tessera", dvp->v_mount, &tessera_vnodeops, &cvp);
	if (e != 0) return (e);
	(void)vn_lock(cvp, LK_EXCLUSIVE | LK_RETRY);
	struct tessera_node *cn = malloc(sizeof(*cn), M_TESSERA,
	    M_WAITOK | M_ZERO);
	cn->inode_no = child_no;
	cvp->v_data = cn;
	/* Type filled in by vop_getattr from the on-disk record; leave
	 * VNON for now and let the kernel re-resolve. */
	cvp->v_type = VNON;
	VN_LOCK_ASHARE(cvp);
	if (insmntque1(cvp, dvp->v_mount) != 0) {
		cvp->v_data = NULL;
		cvp->v_op = &dead_vnodeops;
		vgone(cvp);
		vput(cvp);
		free(cn, M_TESSERA);
		return (EIO);
	}
	vn_set_state(cvp, VSTATE_CONSTRUCTED);
	*vpp = cvp;
	return (0);
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

			/* Slice intersection of [cstart, cend) and
			 * [uio_offset, uio_offset + resid). */
			const uint64_t lo = ((uint64_t)uio->uio_offset > cstart)
			    ? (uint64_t)uio->uio_offset - cstart : 0;
			const uint64_t hi_off =
			    (uint64_t)uio->uio_offset + (uint64_t)uio->uio_resid;
			const uint64_t hi = (hi_off < cend
			    ? hi_off - cstart : cr.uncompressed_size);
			const size_t   n_copy = (size_t)(hi - lo);

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

static int
tessera_vop_open(struct vop_open_args *ap)
{ (void)ap; return (0); }

static int
tessera_vop_close(struct vop_close_args *ap)
{ (void)ap; return (0); }

static int
tessera_vop_reclaim(struct vop_reclaim_args *ap)
{
	struct vnode *vp = ap->a_vp;
	struct tessera_node *tn = VTOTNODE(vp);

	if (tn != NULL) {
		free(tn, M_TESSERA);
		vp->v_data = NULL;
	}
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

struct vop_vector tessera_vnodeops = {
	.vop_default = &default_vnodeops,
	.vop_access  = tessera_vop_access,
	.vop_getattr = tessera_vop_getattr,
	.vop_lookup  = tessera_vop_lookup,
	.vop_readdir = tessera_vop_readdir,
	.vop_read    = tessera_vop_read,
	.vop_open    = tessera_vop_open,
	.vop_close   = tessera_vop_close,
	.vop_reclaim = tessera_vop_reclaim,
};
VFS_VOP_VECTOR_REGISTER(tessera_vnodeops);

VFS_SET(tessera_vfsops, tessera, 0);
MODULE_VERSION(tessera_fs, 1);
