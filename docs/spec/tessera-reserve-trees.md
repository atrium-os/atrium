# Adding a B-tree to the Tessera meta reserve — the registration checklist

Status: normative. Last verified against the tree at c391f09 (2026-08-05).

A tree whose nodes live in the metadata reserve is not finished when it reads
and writes correctly. It is finished when **every component that walks, moves,
or validates the reserve knows it exists.** Miss one and the tree's root sector
gets handed to another tree, the superblock keeps pointing at it, and the
damage is silent until a mount warns about a node kind nobody can explain.

This has now happened three times, once per tree ever added to the reserve:

| tree | added | what was missed | found by | cost |
|---|---|---|---|---|
| blob→pack index | #61 | pinscan did not pin its nodes | #64, GC ate the index | index rebuilt |
| quota | earlier | pinscan roots table had 9 entries, not 10 | #115, a per-mount warning ignored for weeks | all quota domains lost on the dev root |
| dead-extent | #114 | repack's live set, and `commit_roots_t` entirely | #115 follow-up, deliberate experiment | repack corrupted **every** volume that had taken a deferred append |

Three for three. **The omission is the default outcome, not bad luck** — each
of these was written by someone who understood the tree perfectly and simply
did not know the list below existed. That is what this file is for.

## The invariant

> Every superblock root must point at a node **of its own tree kind**, at all
> times, on every path, including immediately after a crash.

`tessera-fsck` enforces exactly this (see the stale-root sweep, `fsck.rs:518`).
When it fails, the sector was recycled into a different tree — which means the
old tree's contents are *destroyed*, not merely unreachable. There is no
recovery for the contents; the only repair is to clear the root and accept the
loss, and for `inode_root` or `pack_registry_root` that is the filesystem.

## The checklist

Adding a tree means touching all seven. Do them in this order — each later item
assumes the earlier ones.

### 1. Format — name the root and its kind

- `core/include/tessera/format.h`: a `<name>_root` + `<name>_gen` pair in
  `tessera_superblock_t`, carved from `reserved[]` so old volumes decode it
  as 0, and `reserved[]` shrunk by exactly 16.
- A `TESSERA_BTREE_KIND_*` constant, plus `*_KEY_SIZE` / `*_VAL_SIZE`.
- Keep the superblock at exactly 4096 bytes. There is a static assert; it is
  load-bearing, because `tessera_encode_superblock` is a raw `memcpy` of the
  packed struct (`codec.c:65`) and every offset is an on-disk ABI.

**0 must mean "absent" and be legal.** Every pre-existing volume has this root
at 0, and everything below must treat 0 as "nothing to do" rather than as a
sector number.

### 2. Pinscan — the GC must not free the tree's nodes

`kmod/tessera_fs.c`, the roots table around line 13560. Add an entry **and
raise the loop bound**:

```c
{ r_<name>, TESSERA_BTREE_KIND_<NAME>, <ksz>, <vsz> },
...
for (int i = 0; i < 10 && !aborted; i++) {   /* ← this number */
```

★ The bound is a separate edit from the table, in a different statement, and
nothing connects them. A table entry past the bound is invisible and compiles
clean. This is precisely how the quota tree was missed (#115).

### 3. Repack — the tree MOVES, it is not a spectator

`rs/tessera-tools/src/bin/repack.rs`, five sites, all required:

| site | what breaks if skipped |
|---|---|
| `struct Trees` + `existed` tuple (`:128`) | entries never read into RAM |
| `struct Built` (`:138`) | no field to carry the new root |
| `build_all` (`:151`) | Phase-B compaction drops the tree |
| `specs` table + every `match victim` arm (`:274`) | staged path skips it |
| both `live_nodes` sweeps (`:376`, `:505`) | **nodes offered as free staging space, bump lowered past them** |
| `commit` (`:219`) | superblock keeps the pre-move root |

The last two are the corrupting ones. Missing `live_nodes` is what let repack
recycle sector 268 into a blob-index node; missing the commit is what left the
superblock pointing there.

★ Make every `match victim` arm explicit and end with `unreachable!()`. A
`_ =>` catch-all silently absorbs the new index and writes your root into the
*previous* tree's superblock field — self-inflicting the exact failure this
document is about.

### 4. commit_roots — the primitive must be able to write the root

`core/include/tessera/volume.h`. Add the field to `tessera_commit_roots_t`
**behind an opt-in flag**:

```c
#define TESSERA_COMMIT_<NAME>  0x…   /* apply roots-><name>_root */
```

Callers written before the field existed must keep preserving the tree by
doing nothing. Without the flag, any caller that zero-fills or partially
initialises the struct destroys a live tree — and there is precedent for the
comment lying about this: `blob_index_root` was documented as "0 keeps
current" when the code compares-and-assigns, so a 0 zeroes it.

Mirror the field and flag in `rs/tessera-sys/src/lib.rs`; field order must
match the C struct exactly.

### 5. fsck — detect a stale root, and say what its loss costs

`rs/tessera-tools/src/bin/fsck.rs:518`. Add a row to the sweep table with a
`StaleFix` tier and a *consequence string*. "kind mismatch" does not tell an
operator what they lost; "all per-directory quota domains are lost; limits stop
being enforced" does.

Pick the tier honestly:

- **Rebuild** — the tree is fully reconstructible from other on-disk state.
  Only `free_extent_root` qualifies today (derived from the pack zone).
- **Clear** — the loss is bounded and nameable, and the volume stays
  consistent and mountable.
- **Refuse** — clearing destroys the filesystem. `inode`, `pack_registry`,
  `snapshots`.

If the tree is *derivable*, say from what, and print the command that rebuilds
it (`tessera-reindex` does this for the blob index). If it is **not** derivable,
it must not be silently dropped anywhere — that is a permanent space or
correctness leak with no tool able to find it again. The dead-extent log is the
worked example: it looks droppable like the blob index, but nothing can
recompute it, so repack moves it instead of clearing it.

Also: if a stale root makes the tree unreadable, **skip that tree's audit**.
Otherwise fsck compares live state against nothing and "repairs" records that
do not exist.

### 6. Any other reserve walker

Today: mount-time validation, the GC live-set walk, and `tessera-defrag`'s
report. Grep for an existing tree's kind constant and read every hit —
`grep -n TESSERA_BTREE_KIND_QUOTA` across `kmod/` and `rs/` is the fastest way
to find the sites this list has not yet learned about.

### 7. Prove it, on a volume that actually has one

A tree with no entries exercises none of the above: every root is 0, every walk
is a no-op, and the whole checklist passes vacuously. Build the state first,
then gate on it:

```sh
# 1. make the tree non-empty, and PROVE it before measuring anything
sysctl -n kern.tessera.<counter>          # before
…workload…
sysctl -n kern.tessera.<counter>          # must have moved

# 2. read the root straight off the superblock (offset = its struct offset;
#    the SB is a raw memcpy, so this is exact). Validate the offset against a
#    root whose value you already know — e.g. one fsck just cleared to 0.
dd if=$DEV bs=4096 count=1 | dd bs=1 skip=<off> count=8 | od -An -tu8

# 3. repack, then re-read. The root must have MOVED, not survived.
# 4. fsck: CLEAN.  5. mount: no kind warning, contents intact.
```

Step 3 is the one that catches the repack class of bug, and only a *moved*
root proves it — an unchanged root is exactly what the broken tool produced.

## Why a checklist and not a test

A test would be better. The obstacle is that each of these registration points
lives in a different language and process (kmod C, offline Rust tools, the
core library), and the failure is a *missing* entry — there is nothing to
assert against without first enumerating what should exist. The realistic
mechanised version is a single generated table of reserve trees that all three
consumers read, which is worth doing and has not been done. Until then this
file is the enumeration.

## See also

- `docs/spec/tessera-fs.md` §3.3 — the metadata reserve on-disk layout
- `docs/spec/tessera-fs.md` §20.2 — the `deferred` dedup policy that produces
  dead extents
- Commits: e960b79/d53e4aa/35ca1fe (#114 stack overflows), 0e27ab8 (quota
  pinscan), 61ab2cb (fsck stale-root repair), c391f09 (repack dead-extent)
