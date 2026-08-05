# Adding a B-tree to the Tessera meta reserve — the registration checklist

Status: normative. Last verified against the tree at 17ab219 (2026-08-05):
generated pinscan A/B'd in-VM against the hand-written one (identical node
sets, no kind warnings), fsck + repack re-verified on a damaged and a clean
volume.

A tree whose nodes live in the metadata reserve is not finished when it reads
and writes correctly. It is finished when **every component that walks, moves,
or validates the reserve knows it exists.** Miss one and the tree's root sector
gets handed to another tree, the superblock keeps pointing at it, and the
damage is silent until a mount warns about a node kind nobody can explain.

This happened three times, once per tree ever added to the reserve. The list
is now generated from one header (see *What is mechanised* below), so the
specific failure below cannot recur — but the history is why the rest of this
document is worth reading:

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

**Start here: add one row to `core/include/tessera/reserve_trees.h`.** That
row is what steps 2, 3 (partly) and 5 below consume — they are generated from
it and need no edit of their own. The remaining steps are still manual.

```c
X(<name>_root, TESSERA_BTREE_KIND_<NAME>, <ksz>, <vsz>, <TIER>,
  "what the operator loses if this root goes stale")
```

Sizes must be integer literals or plain `#define`d constants from `format.h`
— the Rust generator resolves them textually and refuses to guess at
expressions.

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

### 2. Pinscan — GENERATED, nothing to do

`kmod/tessera_fs.c` expands `TESSERA_RESERVE_TREES(X)` into its roots table,
and both the array size and the loop bound derive from it:

```c
roots[TESSERA_RESERVE_TREE_COUNT + 3] = { ... };   /* +3 = GC frozen roots */
for (int i = 0; i < (int)nitems(roots) && !aborted; i++)
```

★ It is worth knowing what this replaced, because the shape recurs elsewhere:
the table, the array dimension and the loop bound were **three independent
numbers with nothing connecting them**, so a row added past the bound compiled
clean and never ran. That is exactly how the quota tree was missed (#115).
Any time you write a literal bound next to a literal table, you have rebuilt
that bug.

The GC's three frozen roots stay hand-written: they are a snapshot of roots
already superseded on disk, not superblock state, so they are deliberately not
in the header.

### 3. Repack — the tree MOVES, it is not a spectator

`rs/tessera-tools/src/bin/repack.rs`, five sites, all required:

| site | generated? | what breaks if skipped |
|---|---|---|
| both `live_nodes` sweeps | **yes** — `RESERVE_TREES` | nodes offered as free staging space, bump lowered past them |
| `struct Trees` + `existed` tuple | no | entries never read into RAM |
| `struct Built` | no | no field to carry the new root |
| `build_all` | no | Phase-B compaction drops the tree |
| `specs` table + `match victim` arms | no | staged path skips it |
| `commit` | no | superblock keeps the pre-move root |

The live sweeps were the corrupting ones — missing one is what let repack
recycle sector 268 into a blob-index node — and they are now generated. What
is left un-generated can only fail to COMPACT your tree, which costs space and
latency, not integrity.

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

GENERATED — the sweep iterates `RESERVE_TREES`. What you must get right is the
**tier and the consequence string in your header row**, because nothing can
infer those. "kind mismatch" does not tell an operator what they lost; "all
per-directory quota domains are lost; limits stop being enforced" does.

Pick the tier honestly:

- **Rebuild** — the tree is fully reconstructible from other on-disk state.
  Only `free_extent_root` qualifies today (derived from the pack zone).
- **Clear** — the loss is bounded and nameable, and the volume stays
  consistent and mountable.
- **Refuse** — clearing destroys the filesystem. `inode`, `pack_registry`,
  `snapshots`.

All three are verified against deliberately damaged volumes (scratch/
tessera-damage-root.c puts a root into the stale state; `dd` cannot, because
the superblock is CRC-covered). Rebuild recovers the free-extent tree to the
exact pre-damage free-space figure; Clear repairs quota / blob-index /
dead-extent and re-verifies CLEAN; Refuse leaves the root untouched, exits 1,
and is a strict no-op — restoring the original root value afterwards returns
the volume to CLEAN with its files intact.

★ A Refuse must never suggest `tessera-repack`. Repack rewrites the metadata
reserve using the trees the volume has just lost, so on a destroyed inode or
pack-registry root it is the worst available next step. fsck's generic
"…or the reserve is exhausted, run tessera-repack" hint is suppressed when a
Refuse-tier root is the reason no progress was made.

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

## What is mechanised, and what is still on you

The list is now **data**: `core/include/tessera/reserve_trees.h` holds one
X-macro table, and the consumers expand or generate from it.

| consumer | how it gets the list |
|---|---|
| kmod pinscan | expands the X-macro directly; array size and loop bound are both derived from it |
| fsck stale-root sweep | `RESERVE_TREES`, generated by `tessera-sys/build.rs` |
| repack live-node sweeps (both) | same generated slice |
| root accessors | `reserve_tree_root()`, generated arms — so no consumer hand-maps field names |

So **steps 2, 5 and the corrupting half of step 3 are automatic**: add a row
and they all pick it up. The build script parses the same header on every
build, so the C and Rust views cannot drift; if it cannot resolve a column it
fails the build rather than guessing.

Still hand-written, and still yours to do:

- **Step 1** (format.h) — nothing can generate a superblock field for you.
- **Step 4** (`commit_roots_t` + its flag) — a C struct the tools fill in.
- **Repack's per-tree MOVE** (`Trees`, `Built`, `build_all`, the `specs`
  table). Forgetting these now costs a tree that is never *compacted* — a
  space and latency issue, not corruption, because the generated live sweeps
  already pin its nodes. That is the point of mechanising the sweeps first:
  the failure mode drops from "silently destroys the tree" to "misses an
  optimisation".
- **Step 7** (prove it on a real volume). Unchanged: nothing here tests that
  your tree actually round-trips.

The remaining generation gap is `Trees`/`Built`, which are per-tree struct
fields rather than table rows. Generating them means either a macro-built
struct or a proc-macro; neither is obviously worth it while the failure is
non-corrupting.

## See also

- `docs/spec/tessera-fs.md` §3.3 — the metadata reserve on-disk layout
- `docs/spec/tessera-fs.md` §20.2 — the `deferred` dedup policy that produces
  dead extents
- Commits: e960b79/d53e4aa/35ca1fe (#114 stack overflows), 0e27ab8 (quota
  pinscan), 61ab2cb (fsck stale-root repair), c391f09 (repack dead-extent)
