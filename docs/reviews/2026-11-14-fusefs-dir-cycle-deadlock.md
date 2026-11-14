# fusefs: a server can make the directory graph cyclic and wedge the kernel

Found 2026-11-14 by `scripts/vm-fuse-fuzz.sh` (seed 2002, second round with
the fuzzer remounting after session-ending replies), cloud amd64 VM, stock
GENERIC 16-CURRENT main-n290812-769c49de261f.

## What happens

The hostile server answered a LOOKUP in directory 25 with node id 1, the
root. fusefs only refuses a child that has the *same* node id as its parent
(`fuse_vnode_get`, "Assigned same inode to both parent and child"); an
ancestor passes. With root -> 16 -> 25 -> root in the graph, three lookups
walking it each held one directory's vnode lock and waited for the next:

```
vnode nodeid 1  (root)   EXCL by pid 1864, which waits in cache_lookup/vget
vnode nodeid 16 (dir)    EXCL by pid 1866, which waits in cache_lookup/vget
vnode nodeid 25 (dir)    EXCL by pid 1865, which waits in fuse_vnode_get/vfs_hash_get
```

(`show lockedvnods`, scratch capture). These are lockmgr sleeps, not FUSE
answer waits, so the `timeout=` mount option does not help, `SIGKILL` does
not end them, and killing the server does not either: the processes stay in
`D` for good and the mount can't be unmounted. Anything else that walks
into those directories joins them. For Atrium that means a compromised
share server (network-shares.md §4.4) could leave apps that touched the
share stuck until reboot. Release kernels are affected the same way.

## Reproducer

`atrium-fuse-hostile --mnt /tmp/fz/mnt --secs 20 --drivers 2 --mutate 0
--repro dir-cycle`: an honest server except that `up` in any directory is
the root; one driver walks `dir1/up/dir1/up/dir1/a`, the other `dir1/a`.
On the stock kernel both drivers are stuck in `D` (wchan `fuse`) within
seconds and stay there after the server is gone.

## Fix (fork, `fbsd:52048d225`)

`fuse_vnode_get()` refuses (EIO, warn once per mount: "Assigned a
directory's ancestor as its child") a node id that is the root or one of
`dvp`'s recorded ancestors. The walk follows `parent_nid` through vnodes
that still exist, each found with a new `vfs_hash_hold()` (held, not
referenced, read under the interlock), so it takes no vnode lock and can't
itself deadlock. New fusefs test `Lookup.ancestor_inode`.

Limits: the walk sees only ancestors whose vnodes are still in the vnode
hash. A directory with name-cache entries under it stays held, so the chain
above a busy directory is normally there, but under vnode pressure an
ancestor can be recycled and the walk stops early: this is a strong
mitigation, not a proof. A directory moved on the server under its former
descendant, with the kernel still holding the old layout, gets EIO until it
is looked up again from its new place.

## Tested

Patched GENERIC (INVARIANTS, WITNESS) in the cloud VM, with all three fixes: the reproducer's
`dir1/up` fails with EIO and nothing hangs; `Lookup.ancestor_inode` and the
rest of `lookup` pass (18/18, built on the VM against the stock gtest); the
stock fusefs suite passes (690/771, 0 failed, 81 skipped). Not reported
upstream yet (Shankar: report later).
