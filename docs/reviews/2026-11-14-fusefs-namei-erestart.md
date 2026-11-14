# fusefs: a killed lookup through a symlink panics INVARIANTS kernels

Found 2026-11-14 by the hostile FUSE server (`atrium-fuse-hostile`,
network-shares.md §4.4 item 1) on its first mutating run, cloud amd64 VM,
stock GENERIC 16-CURRENT main-n290812-769c49de261f (INVARIANTS, WITNESS).

```
panic: Assertion error != 0 && error != ERESTART failed at /usr/src/sys/kern/vfs_lookup.c:905
namei() <- kern_symlinkat()     (fuzz run, seed 2)
namei() <- kern_statat()        (reproducer)
```

## Cause

`namei()` handles an `ERESTART` from `vfs_lookup()` by restarting the walk,
and asserts at `out:` that no `ERESTART` gets past it. But the other way to
`out:` is `namei_follow_link()` failing, and its `VOP_READLINK()` can sleep
interruptibly. fusefs does: `fticket_wait_answer()` sleeps with `PCATCH`
(all signals but `SIGKILL` blocked unless the server does `FUSE_INTERRUPT`
and the mount has `intr`), and a `SIGKILL` while it waits returns
`ERESTART`. NFS with `intr` is the same shape.

So: a process walks a path through a symlink on a FUSE mount, the server is
slow to answer the `READLINK`, and the process is killed. Any user who can
kill their own process while a FUSE server stalls can trigger it, so on a
debug kernel it is a local denial of service; a hostile or merely slow
server makes it easy.

Without INVARIANTS the assertion is compiled out and `namei()` returns
`ERESTART`; the thread is exiting (or, for a caught signal with
`SA_RESTART`, the call restarts), which is the right outcome. Atrium's
release kernels are not affected; debug and KASAN kernels are.

## Reproducer

`atrium-fuse-hostile --mnt /tmp/fz/mnt --secs 20 --drivers 1 --mutate 0
--repro readlink-kill` (as root in the VM): an otherwise honest server that
holds the first `READLINK`, a driver doing `stat("mnt/lnk/x")`, and a
`SIGKILL` to the driver while it waits. Panics every time (2 of 2).

## Fix

Let the assertion accept the `ERESTART` that only `namei_follow_link()` can
produce. Patch against upstream main (line 933 there):

```diff
--- a/sys/kern/vfs_lookup.c
+++ b/sys/kern/vfs_lookup.c
@@ -930,7 +930,14 @@
 	ndp->ni_vp = NULL;
 	vrele(ndp->ni_dvp);
 out:
-	MPASS(error != 0 && error != ERESTART);
+	/*
+	 * vfs_lookup()'s ERESTART is handled above, but VOP_READLINK() in
+	 * namei_follow_link() may sleep interruptibly (fusefs waiting on its
+	 * daemon, NFS with intr) and fail with ERESTART when a signal
+	 * arrives.  That one is passed up, so the system call restarts or
+	 * the thread exits.
+	 */
+	MPASS(error != 0);
 	SDT_PROBE4(vfs, namei, lookup, return, error, NULL, false, ndp);
 	namei_cleanup_cnp(cnp);
 	nameicap_cleanup(ndp, error);
```

Status: applied to the fork as `fbsd:a7f02efce` (Shankar 2026-11-14:
"Apply it. We can report it later"). On the patched GENERIC kernel the
reproducer runs clean and the stock fusefs suite passes (690/771, 0 failed,
81 skipped). Not reported upstream yet. The fuzz rounds still stop their
drivers with `SIGTERM`, which fusefs holds until the answer.
