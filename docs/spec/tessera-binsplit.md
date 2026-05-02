# Tessera-binsplit — function-level dedup of native binaries

Status: design + Phase 1 research.
Last updated: 2026-05-03.

## 0. The problem

File-level dedup (Tessera CAS today) doesn't help much for Rust
binaries because Rust monomorphizes generics, and the resulting
machine code differs between binaries by:
- different addresses (relocations resolved differently)
- different surrounding-code-driven inlining decisions (LTO)
- different ICF (`rust-lld --icf=safe`) outcomes per binary
- different debuginfo

Two Atrium apps both depending on `serde`, `tokio`, `regex` will
each carry their own copy of those crates' code. SHA-256 of either
binary won't match anything in the other → Tessera dedups nothing
useful.

## 1. The architectural position (re-affirmed)

> "Function-level dedup as a **storage primitive** makes the
> linking-strategy question moot — you collect dedup wherever
> functions happen to be byte-identical, you eat duplicates
> wherever they aren't, and Rust gets to keep its native model."

**No FreeBSD ABI change needed.** No new dynamic linker. No Rust
ABI requirements. Pure userspace tooling that operates on
already-linked native binaries:

1. Walk the binary's symbol table.
2. For each function: extract the bytes, identify and strip
   relocations to a side table, hash the normalized bytes.
3. Store function blobs in Tessera CAS keyed by their hash.
4. Store a per-binary "recipe" that lists (function_hash,
   relocation_table, layout_offset) so the binary can be
   reconstituted.
5. At install / first-launch / exec time, materialise the
   reconstituted binary and exec it.

Functions that happen to be byte-identical across binaries
share storage. Functions that differ (most monomorphizations)
get stored separately, no dedup. The *storage tier* absorbs the
non-determinism Rust permits.

## 2. Architecture

### 2.1 The function-blob format

```
function_blob = sha256(
    bytes_with_relocation_targets_zeroed
)
```

A function blob is its machine code with all relocation-affected
bytes overwritten with zeros (or another sentinel). Two functions
with identical instructions but different absolute addresses for
their callees produce the same blob hash.

Stored in Tessera CAS as ordinary blobs. Same hash space as
everything else; aggregation packs (the recent multi-blob pack
work) handle the small-blob case efficiently.

### 2.2 The recipe format

Per binary, we store a "recipe" describing how to reassemble:

```
recipe {
    arch:       ("aarch64-freebsd" | "x86_64-freebsd")
    elf_header: Vec<u8>    // verbatim ELF + program headers + sections (minus .text)
    sections:   Vec<{ name, virtual_address, contents_hash | inline_bytes }>
    functions:  Vec<{
        symbol_name:   Option<String>   // for diagnostics; not required
        virtual_address: u64
        function_blob: Hash             // CAS blob = normalized function bytes
        relocations:   Vec<{
            offset_in_function: u32
            relocation_type:    u32     // ELF reloc type (R_AARCH64_*)
            target:             RelocTarget
            addend:             i64
        }>
    }>
    entry_point: u64
    metadata:   { rustc_version, build_flags, etc. }   // for debugging only
}
```

`RelocTarget` enumerates: another function (by `function_blob` +
offset), a section symbol (.rodata, .data, .bss with offset), an
external dynamic symbol (libc `malloc`, etc.).

The recipe itself is a CAS blob. A "package" then becomes:
a few CAS hashes (recipe + sections + per-function blobs) and
some metadata.

### 2.3 The reconstitutor

```
reconstitute(recipe) -> Vec<u8> (an ELF binary)

  1. Allocate output buffer of recipe.elf_size.
  2. Copy elf_header verbatim.
  3. For each section: fetch contents_hash from CAS, copy into
     section's virtual address.
  4. For each function:
     a. Fetch function_blob from CAS.
     b. Copy into virtual_address.
     c. For each relocation: compute target address, patch into
        the function bytes at offset_in_function (using addend +
        relocation_type's encoding rules).
  5. Patch entry_point into ELF header.
  6. Set page protections (RX for text, RW for data, etc.).
```

For Phase 4+ (production use), we materialise once at install
or first-launch and cache the result; we don't pay the
reconstitute cost on every exec.

### 2.4 The install-time pipeline

```
tessera-import --binsplit <src_tree> <dst>
    walks src_tree looking for ELF binaries
    for each binary:
        extract = binsplit(binary) -> (recipe, function_blobs[], section_blobs[])
        upload each blob to Tessera CAS (auto-dedups via the
            existing pack_registry shortcut)
        store recipe + a small loader stub at dst path
    non-ELF files: copy as today
```

Non-Rust binaries (C apps with dynamic linking) benefit too —
their text segments get function-deduped, their static-data
sections get content-deduped, the dynamic libs they link against
were already shared via the OS dynamic linker.

### 2.5 The exec path

Two options:

**Option A: shim loader** — in place of the real binary, install
a small loader that reads the recipe, reconstitutes into an
anonymous file (memfd / SHM_ANON on FreeBSD), and `fexecve`'s
it. First exec materialises; subsequent execs of the same
binary reuse the per-app cached materialisation.

**Option B: kernel hook** — `binmiscctl`-style imgact registers
a "tessera-recipe" magic; kernel detects and reconstitutes into
a kernel-managed cache, exec proceeds as normal. Cleaner UX but
requires more privileged code.

Phase 1-3 don't need to decide — both are doable later.

## 3. Why this is hard (honest list)

- **Position-dependent code.** PC-relative loads are common in
  aarch64. They're already position-relative within a function
  → no relocation needed → fine. But PC-relative loads to
  *outside* the function (a constant in .rodata, another
  function) need relocation tracking.
- **Embedded data in .text.** Some functions have inline jump
  tables, switch tables, or constant pools. ELF tells us where
  via DWARF, sometimes. Without DWARF or with `strip`-ped
  binaries, we may misclassify data as code.
- **LTO erases function boundaries.** If `f` was inlined into
  `g`, `f` doesn't exist as a separate function in the output.
  The dedup unit is "what the linker emitted as a separate
  symbol." Inlined code can't be deduped.
- **Vendored dependencies.** Two Rust binaries each statically
  linking a different version of the same crate produce
  different bytes. We dedup what's identical, not what's
  semantically equivalent.
- **Per-arch relocation handling.** aarch64 has ~40 relocation
  types; x86_64 has ~25. Each needs encode/decode logic.
- **Rust ABI instability across compiler versions.** Same source
  + different rustc → different bytes. Reproducible builds help
  but aren't universal.
- **Symbol stripping.** `cargo build --release` doesn't strip,
  but `pkg`-distributed binaries often do. Without symbols, we
  can't identify function boundaries reliably (DWARF .debug_info
  has them; if both are stripped, we use heuristics or refuse
  to binsplit).

## 4. Phased implementation plan

**Phase 1 (research tool) — `tessera-binsplit --analyze` — THIS MVP.**

Just statistics, no extraction or reconstitution. Walk a binary,
identify functions, hash each (with relocation bytes zeroed),
print stats. Run against several real Rust binaries (atrium-edit-
socket, atrium-textured, atrium-keyboard, atrium-broker eventually).
Compute pairwise dedup rates: "binary A and binary B share N
function blobs totaling M bytes."

If the numbers are encouraging (≥30% of code mass is dedup-able
across our own apps), proceed. If the numbers are weak (<10%),
revisit the approach (maybe push for `-Csymbol-mangling-version`
or `-Crelocation-model=pic` to improve dedup-ability; maybe
accept that file-level dedup of static data is the realistic
ceiling).

**Phase 2 — extractor + recipe builder.**

`tessera-binsplit --extract <binary> <out_dir>` writes:
- `out_dir/recipe.cbor` — the recipe
- `out_dir/blobs/<hash>` — one file per unique function blob
- `out_dir/sections/<hash>` — section contents

Designed so `tessera-import` can shovel `out_dir/blobs/*` and
`out_dir/sections/*` into a Tessera volume; the recipe is
itself a CAS blob.

**Phase 3 — reconstitutor.**

`tessera-binsplit --reconstitute <recipe.cbor> <out_binary>`
that produces a byte-identical (or functionally identical, if
we accept reordering) copy of the original binary. Round-trip
verification: `binsplit && reconstitute && diff` should produce
zero diff for any input we accept.

**Phase 4 — install-pipeline integration.**

`tessera-import` gains a `--binsplit` flag that auto-detects
ELF binaries and routes them through extract → store. Recipes
land at the original file path; the loader stub knows how to
exec them.

**Phase 5 — exec-path integration.**

Either option A (shim loader) or option B (imgact hook). First
launch materialises into a per-app cache; subsequent launches
reuse. Per-app cache itself can live in Tessera (CoW, snapshotted,
gc'd along with everything else).

D1.5 is "complete" today; this work would be **D1.5+ (a follow-on
phase)** that extends Tessera's storage value to native binaries.
Not blocking D2.5 / D2 / D3.

## 5. Data we want from Phase 1

For each input binary, output:
- total .text bytes
- function count
- bytes accounted for by extracted functions
- bytes NOT accounted for (data in .text, padding, ICF-folded
  ranges)
- distribution of function sizes
- top-10 largest functions
- top-10 most-relocated functions

For each pair of binaries:
- shared function blobs (count + total bytes)
- unique-to-A blobs
- unique-to-B blobs
- effective dedup ratio if both were stored split

Decision threshold to proceed to Phase 2: ≥30% byte-mass dedup
between two real apps in our tree (atrium-edit-socket vs
atrium-textured, say). Below that, the engineering cost of
Phase 2-5 is hard to justify against just shipping fatter
binaries with file-level dedup of their data sections.

## 5.1 Phase 1 results (2026-05-03)

`tessera-binsplit` (aarch64 PC-rel masking enabled — see
`tessera-tools/src/bin/binsplit.rs`). Tested against the existing
Atrium app binaries.

Within-binary stats (typical):
- atrium-rect-bouncer: 286 KiB .text, 1957 functions, 132 within-
  binary dup funcs (already ICF-folded).
- atrium-edit-socket: 1.24 MiB .text, 2089 functions, larger because
  of fresco-text/swash/skrifa pulled in for text rendering.

**Cross-binary dedup with PC-rel masking:**

| Pair | Shared bytes / smaller binary |
|---|---|
| rect-bouncer ↔ keyboard (same workspace)   | 71% |
| rect-bouncer ↔ window-demo (same workspace)| 70% |
| rect-bouncer ↔ textured (same workspace)   | 71% |
| rect-bouncer ↔ edit-socket (cross workspace)| 49% |
| edit-socket ↔ textured (cross workspace)   | 51% |

Without PC-rel masking the shared rate was ~13% — confirms that
PC-rel offsets are most of what makes "the same logical function"
look different across binaries.

**Decision:** ≥30% threshold from §5 met by all measured pairs.
**PROCEED to Phase 2.**

Caveat: results are for binaries built with the same rustc + same
build flags, in the same workspace context. Cross-version /
cross-toolchain dedup will be lower; reproducible-builds discipline
is required to capture full value.

### 5.2 N-way aggregate (all 9 Atrium binaries)

`tessera-binsplit --multi <bin>...` over the full set of currently-
built Atrium app binaries:

```
                      flat sum   function-deduped   saved
                      4.28 MiB         2.26 MiB     2.02 MiB (1.89×)
```

Marginal install cost (cumulative, in argv order):

| Binary | Flat | Marginal | % Saved |
|---|---|---|---|
| atrium-rect-bouncer | 280 KiB |  266 KiB |  5% |
| atrium-keyboard     | 250 KiB |   70 KiB | 72% |
| atrium-window-demo  | 269 KiB |   76 KiB | 72% |
| atrium-textured     | 267 KiB |   74 KiB | 72% |
| atrium-mouse-demo   | 267 KiB |   74 KiB | 72% |
| atrium-slot-demo    | 281 KiB |   77 KiB | 73% |
| atrium-test-client  | 265 KiB |   72 KiB | 73% |
| atrium-text-demo    | 1.21 MiB | 1.01 MiB | 17% |
| atrium-edit-socket  | 1.24 MiB |  573 KiB | 55% |

**The per-app marginal cost converges to ~70-75 KiB** for apps
in the same workspace using overlapping dep sets. The text-rendering
binaries (text-demo, edit-socket) add a one-time ~1 MiB chunk for
swash/skrifa/etc., then subsequent text apps share most of that.

This is the actual user-visible value: installing N Atrium apps
costs ~1.5–2× one app, not Nx.

## 6. Non-goals

- Not a JIT. Reconstitution produces ordinary native code that
  the FreeBSD loader execs.
- Not a hot-patch system. Recipes are immutable; updating a
  function means a new recipe.
- Not cross-arch. aarch64-FreeBSD recipes don't reconstitute
  on x86_64. Per-arch CAS namespaces (or a `arch` field in the
  hash domain).
- Not a security boundary. A malicious recipe could produce
  arbitrary code; signing happens at the package layer
  (Portcullis manifest signature, not at binsplit).
