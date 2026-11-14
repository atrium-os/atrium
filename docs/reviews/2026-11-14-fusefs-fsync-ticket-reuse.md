# fusefs: fsync reuses a ticket that is still queued

Found 2026-11-14 by `scripts/vm-fuse-fuzz.sh` (seed 3005) on the kernel
already carrying the namei and directory-cycle fixes; cloud amd64 VM,
GENERIC with INVARIANTS.

```
panic: FUSE: ticket still on answer delivery list 0xfffff80141cf0da8
fdisp_refresh_vp() <- fuse_internal_fsync() <- kern_fsync()
```

## Cause

`fuse_internal_fsync()` sends one FSYNC per open handle of the file and
reuses a single ticket for them (`fdisp_refresh_vp()` between handles).
That is only safe once the previous answer has been delivered, which is
not the case when:

- the wait timed out (`timeout=`) or was interrupted: the ticket stays on
  the answer list until the daemon replies, and the loop went on to the
  next handle anyway; or
- `waitfor` was not `MNT_WAIT`: the ticket was queued asynchronously and is
  still on both lists.

Refreshing it puts it on the lists a second time. INVARIANTS asserts;
without INVARIANTS the answer and message lists are corrupted (a ticket
linked twice), which can end in a use after free. A server that doesn't
answer FSYNC on a file that is open twice is enough, so a compromised share
server can do it to a release kernel.

## Reproducer

`atrium-fuse-hostile --mnt /tmp/fz/mnt --secs 15 --drivers 1 --mutate 0
--timeout 2 --repro fsync-timeout`: the server never answers FSYNC; the
driver opens one file read-only and write-only (two handles) and calls
fsync. The stock kernel panics every time.

## Fix (fork, `fbsd:47dfe5c94`)

Each handle gets its own dispatcher, and the loop stops at the first error.
On the patched kernel the reproducer's fsync fails with ETIMEDOUT and
nothing asserts; the stock fusefs suite passes (690/771, 0 failed, 81
skipped); 13 fuzz rounds (seeds 4001-4013) ran clean.
Not reported upstream yet (Shankar: report later).
