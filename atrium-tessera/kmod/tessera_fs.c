/*
 * tessera_fs: FreeBSD VFS module for the Tessera content-addressed
 * filesystem.
 *
 * Phase 0: this is a skeleton. The module registers a VFS named
 * "tessera" but every op returns EOPNOTSUPP. Phase 4 fills in mount,
 * unmount, root, statfs, sync; Phase 5 fills in vnode ops.
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
#include <sys/conf.h>
#include <sys/proc.h>
#include <sys/malloc.h>

static MALLOC_DEFINE(M_TESSERA, "tessera", "Tessera filesystem");

static int
tessera_mount(struct mount *mp)
{
	(void)mp;
	return (EOPNOTSUPP);
}

static int
tessera_unmount(struct mount *mp, int mntflags)
{
	(void)mp; (void)mntflags;
	return (EOPNOTSUPP);
}

static int
tessera_root(struct mount *mp, int flags, struct vnode **vpp)
{
	(void)mp; (void)flags; (void)vpp;
	return (EOPNOTSUPP);
}

static int
tessera_statfs(struct mount *mp, struct statfs *sbp)
{
	(void)mp; (void)sbp;
	return (EOPNOTSUPP);
}

static int
tessera_sync(struct mount *mp, int waitfor)
{
	(void)mp; (void)waitfor;
	return (0);
}

static int
tessera_init(struct vfsconf *vfsp)
{
	(void)vfsp;
	printf("tessera_fs: phase-0 skeleton loaded\n");
	return (0);
}

static int
tessera_uninit(struct vfsconf *vfsp)
{
	(void)vfsp;
	printf("tessera_fs: phase-0 skeleton unloaded\n");
	return (0);
}

static struct vfsops tessera_vfsops = {
	.vfs_mount   = tessera_mount,
	.vfs_unmount = tessera_unmount,
	.vfs_root    = tessera_root,
	.vfs_statfs  = tessera_statfs,
	.vfs_sync    = tessera_sync,
	.vfs_init    = tessera_init,
	.vfs_uninit  = tessera_uninit,
};

VFS_SET(tessera_vfsops, tessera, 0);
MODULE_VERSION(tessera_fs, 1);
