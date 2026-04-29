/*
 * tessera_fs: FreeBSD VFS module for the Tessera content-addressed
 * filesystem.
 *
 * Phase 4 round 1: VFS-plumbing skeleton — registers the "tessera" VFS,
 * supports `mount -t tessera dummy /mnt/x` with no backing store, and
 * exposes a synthesized empty root directory. Round 2 adds the real
 * file-backed mount via the canonical FreeBSD mdconfig + g_vfs_open
 * path.
 *
 * Spec: docs/spec/tessera-fs.md (on-disk format)
 *       docs/spec/tessera-vfs.md (POSIX mapping)
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

#include "tessera/format.h"
#include "tessera/volume.h"

MALLOC_DEFINE(M_TESSERA, "tessera", "Tessera filesystem");

/* ── per-mount state ─────────────────────────────────────────── */

struct tessera_mount {
	uint64_t  total_sectors;
	uint64_t  pack_zone_length;
	uint64_t  generation;
};

#define VFSTOTESSERA(mp) ((struct tessera_mount *)((mp)->mnt_data))

/* ── per-vnode private ──────────────────────────────────────── */

struct tessera_node {
	uint64_t inode_no;
};

#define VTOTNODE(vp) ((struct tessera_node *)(vp)->v_data)

extern struct vop_vector tessera_vnodeops;

/* ── vfs_mount ───────────────────────────────────────────────── */

static int
tessera_mount_impl(struct mount *mp)
{
	struct tessera_mount *tmp_;

	if (mp->mnt_flag & MNT_UPDATE)
		return (EOPNOTSUPP);

	tmp_ = malloc(sizeof(*tmp_), M_TESSERA, M_WAITOK | M_ZERO);
	/* Round 1: hardcode plausible volume parameters. Round 2 reads
	 * these from the on-disk superblock of a backing image. */
	tmp_->total_sectors    = 4096;
	tmp_->pack_zone_length = 4096 - (4 + 256 + TESSERA_METADATA_ZONE_SECTORS);
	tmp_->generation       = 1;

	mp->mnt_data = tmp_;
	mp->mnt_flag |= MNT_LOCAL | MNT_RDONLY;
	MNT_ILOCK(mp);
	mp->mnt_kern_flag |= MNTK_LOOKUP_SHARED | MNTK_EXTENDED_SHARED;
	MNT_IUNLOCK(mp);

	vfs_getnewfsid(mp);
	vfs_mountedfrom(mp, "tessera");

	printf("tessera_fs: mounted (placeholder volume) on %s\n",
	    mp->mnt_stat.f_mntonname);
	return (0);
}

/* ── vfs_unmount ─────────────────────────────────────────────── */

static int
tessera_unmount_impl(struct mount *mp, int mntflags)
{
	struct tessera_mount *tmp_ = VFSTOTESSERA(mp);
	int flags = 0, err;

	if (mntflags & MNT_FORCE) flags |= FORCECLOSE;
	/* rootrefs=0: VFS_ROOT allocates the root vnode lazily on each
	 * call rather than holding one for the lifetime of the mount, so
	 * vflush has no extra refs to drop. */
	err = vflush(mp, 0, flags, curthread);
	if (err != 0) return (err);

	if (tmp_ != NULL) {
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

	vp->v_data = tn;
	vp->v_type = VDIR;
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
	sbp->f_blocks = tmp_->total_sectors;
	sbp->f_bfree  = tmp_->pack_zone_length;
	sbp->f_bavail = tmp_->pack_zone_length;
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

static int
tessera_vop_getattr(struct vop_getattr_args *ap)
{
	struct vnode *vp = ap->a_vp;
	struct vattr *vap = ap->a_vap;
	struct tessera_node *tn = VTOTNODE(vp);

	VATTR_NULL(vap);
	vap->va_type    = VDIR;
	vap->va_mode    = 0755;
	vap->va_nlink   = 2;
	vap->va_uid     = 0;
	vap->va_gid     = 0;
	vap->va_fsid    = vp->v_mount->mnt_stat.f_fsid.val[0];
	vap->va_fileid  = tn->inode_no;
	vap->va_size    = 0;
	vap->va_blocksize = TESSERA_SECTOR_SIZE;
	vap->va_bytes   = 0;
	vap->va_gen     = 1;
	vap->va_flags   = 0;
	return (0);
}

static int
tessera_vop_lookup(struct vop_lookup_args *ap)
{
	struct vnode *dvp = ap->a_dvp;
	struct vnode **vpp = ap->a_vpp;
	struct componentname *cnp = ap->a_cnp;

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
	return (ENOENT);
}

/*
 * dirent record sizing — both "." and ".." encode into the smallest
 * valid record that fits the GENERIC_DIRSIZ alignment (8). The bytes
 * after the name (within the d_reclen window) must be zeroed; we use
 * a stack buffer at the maximum supported reclen and fill the right
 * prefix.
 */
#define DIRENT_HDR  offsetof(struct dirent, d_name)

static size_t
tessera_dirent_reclen(uint16_t namlen)
{
	size_t need = DIRENT_HDR + namlen + 1;     /* name + NUL */
	return ((need + 7) & ~7);                   /* 8-byte align */
}

static int
tessera_emit_dirent(struct uio *uio, ino_t fileno, uint8_t type,
                    const char *name, uint16_t namlen)
{
	uint8_t buf[64];
	size_t reclen = tessera_dirent_reclen(namlen);
	if (reclen > sizeof(buf)) return (EINVAL);

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

static int
tessera_vop_readdir(struct vop_readdir_args *ap)
{
	struct uio   *uio = ap->a_uio;
	struct tessera_node *tn = VTOTNODE(ap->a_vp);
	int err;

	const size_t r1 = tessera_dirent_reclen(1);   /* "." */
	const size_t r2 = tessera_dirent_reclen(2);   /* ".." */

	if (uio->uio_offset == 0 && uio->uio_resid >= r1) {
		err = tessera_emit_dirent(uio, tn->inode_no, DT_DIR, ".", 1);
		if (err != 0) return (err);
	}
	if (uio->uio_offset == (off_t)r1 && uio->uio_resid >= r2) {
		err = tessera_emit_dirent(uio, tn->inode_no, DT_DIR, "..", 2);
		if (err != 0) return (err);
	}
	if (ap->a_eofflag != NULL) *ap->a_eofflag = 1;
	return (0);
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

struct vop_vector tessera_vnodeops = {
	.vop_default     = &default_vnodeops,
	.vop_access      = tessera_vop_access,
	.vop_getattr     = tessera_vop_getattr,
	.vop_lookup      = tessera_vop_lookup,
	.vop_readdir     = tessera_vop_readdir,
	.vop_open        = tessera_vop_open,
	.vop_close       = tessera_vop_close,
	.vop_reclaim     = tessera_vop_reclaim,
};
VFS_VOP_VECTOR_REGISTER(tessera_vnodeops);

VFS_SET(tessera_vfsops, tessera, 0);
MODULE_VERSION(tessera_fs, 1);
