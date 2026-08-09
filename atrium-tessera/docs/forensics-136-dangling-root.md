# #136 — forensic reading of the dangling root directory

Volume: `vm/boottest.img` (kept unmodified). Partition `atrium-root` at LBA
133120. Superblock generation **19797** — the generation at which the dev root
lost its root directory. Everything below was read offline from the raw image;
nothing was mounted.

## What is PROVEN from the image

**1. The committed root inode names a manifest that is in no pack.**
Descending the inode btree (`inode_root=30427`, keys are BIG-endian u32) to
key 2:

    COMMITTED inode 2: gen=80 mode=0o40755 parent=0 size=0 nlink=2
      manifest_hash = 72b526c4bcfe6e22951676d99cd492e490db4494662e63262197426321ef7be8

That hash is ABSENT from the blob index (`blob_index_root=30441`), and the
loader instrumentation in a386bb36 already proved it absent from all 128285
packs.

**2. The committed record is byte-identical to a journal record.**
The ring holds four `INODE_WRITE` records for inode 2:

    seq=211  rec_gen=78  mh=b7bf8f08...
    seq=216  rec_gen=79  mh=45fcd77f...
    seq=221  rec_gen=79  mh=45fcd77f...
    seq=225  rec_gen=80  mh=72b526c4...   <-- identical to the committed inode 2

**3. Of those three root manifests, only the OLDEST survives.**

    gen78 b7bf8f08...  INDEXED
    gen79 45fcd77f...  ABSENT
    gen80 72b526c4...  ABSENT   (and committed)

**4. ★ The journal ring physically holds records from MORE THAN ONE EPOCH.**
This is the finding that settles it. The ring's two `ROOT_UPDATE` records:

    seq=5  generation=19797  inode_root=30427  (== the committed superblock)
    seq=8  generation=19486  inode_root=21474

A LATER sequence number carrying an OLDER generation is impossible within one
epoch. It is only possible because formatting the journal resets
`head_seq`/`tail_seq`/`head_block` (the header here reads head=tail=1) but
**does not erase the ring**. Each session therefore numbers its records from 1
again, and blocks the new session has not yet overwritten still hold the
previous epoch's records — with low sequence numbers of their own.

**Consequence: a sequence number is not an ordering across epochs, and a
replay that walks the ring block-by-block sees a MIXTURE of epochs.**

## What this identifies

The path that wrote the dangling `manifest_hash` into inode 2 is **journal redo
replay** — `tessera_replay_dirent_record` → `tessera_fs_inode_put` — not any
normal commit path. Fact 4 supplies the leftover record, fact 2 supplies the
exact bytes it wrote, and facts 1+3 are what an offline `repack` (which rewrites
metadata IN PLACE and is documented as not crash-safe) leaves behind: the old
epoch's newer root manifests destroyed, an older one still resolvable.

The live flush path is NOT implicated, and was checked rather than assumed.
`tessera_fs_flush` gates strictly in the right order —
`pending_manifests_drain` → `dirty_inodes_drain` → `registry_ov_flush` →
`commit_extent` → `commit_sb`, each on `r == 0` — so an in-order flush cannot
commit an inode ahead of its manifest's pack. The SATB barrier
(`gc_note_hash`) is likewise armed on the inode-drain path
(`rec->manifest_hash`), so a concurrent GC is not implicated either.

This is the mechanism #137 fixed, now confirmed against the artifact instead of
inferred from the "0 ROOT_UPDATE applied, 1 redo re-applied" counters:

- `record.generation < sb.generation` drops the leftover (2175cba6, e76c7156).
  Records from an older epoch carry no generation field at all (body=152 vs
  160), so they now fail the `body_length` check and are dropped outright.
- `tessera-fsck --repair` / `tessera-repack` format the journal after sealing
  new roots, so a repaired volume has no leftover ring to replay.
- Replay refuses an inode whose `manifest_hash` resolves to no pack.

Any ONE of the three would have prevented this. All three shipped.

## Residue worth knowing

Formatting the journal writes only the header. The stale record BYTES remain in
the ring and stay CRC-valid — they are inert only because head==tail bounds
replay and because the generation guard now rejects them. Nothing scrubs them.
That is acceptable but it means the ring is not self-describing: never reason
about journal contents by scanning for `TXR\0` magic, only by walking
head..tail. This document does scan, deliberately, to see the leftovers.
