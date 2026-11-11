# Tessera on-disk decoders in Rust (D3)

**Status:** 2026-11-10. Step 4 of
[the kernel security review](../../docs/reviews/2026-11-07-freebsd-kernel-security-rust.md)
(decision D3), phase 1: every decoder that turns on-disk bytes into structure now runs as
safe Rust, in `tessera_fs.ko` and in the host test build. The C bodies stay as the reference
and for the loader.

## What moved

`rs/tessera-decode` is one `no_std` crate with `#![deny(unsafe_code)]` everywhere except
`ffi.rs`. It holds:

| module | replaces (C) | what it checks |
|--------|--------------|----------------|
| `codec` | `codec.c` `tessera_decode_*` (14 decoders) | superblock, journal header, record header, pack header and footer, blob descriptor, manifest header, B-tree node header, pack extent list (magic, CRC, count 1..253, no zero-length extent, lengths sum to the total) |
| `crc` | `crc.c` (for the decoders) | CRC-32, slice-by-8, const table |
| `manifest` | `manifest.c` parser half | header, body bounds, chunk/tree/bucket/xattr/dirent/dir-btree accessors, and a whole-node check for directory B-tree nodes (`dir_btree_node`) |
| `pack` | `pack.c` reader; the kmod's two pack readers (phase 2) | pack geometry (the #124 checks plus the footer, or from the header sector alone), bloom probe, binary search of the index, descriptor bounds |
| `btree` | `btree.c` `load_node` checks and `search_leaf` | node header, tree kind, fan-out cap, key/value geometry, binary search |
| `journal` | `journal.c` record read and advance | record header, sector count (64-bit), body inside the ring |

The C entry points keep their names. Under `-DTESSERA_RUST_DECODE` each forwards to a
`tessera_rs_*` export (declared in `core/src/tessera_rs.h`); without it the C body is
compiled as before. So no caller changed, and the kernel module can be built either way.

Every access in the crate goes through `get()` or a checked range, so a bad length is an
error return, never a panic. `scripts/tessera-decode-check.sh` proves that on the kernel
targets: it compiles the crate for `aarch64-unknown-none-softfloat` and
`x86_64-unknown-none` and fails if the object references anything other than `memcmp`,
`memcpy` and `memmove` (a panic path would pull in `core::panicking`). It also fails on any
stack frame over 512 bytes. The largest today is 112 bytes on aarch64 and 56 on x86_64. The
C pack extent list decoder used 4160 bytes (kmod audit T2).

## Three ways the crate is built

1. **cargo**, for its own unit tests (`cargo test -p tessera-decode`).
2. **`rs/tessera-decode-c`**, a staticlib for the host C core. `RUST_DECODE=1
   scripts/core-host-tests.sh` runs the whole core suite against it, plain and under
   ASAN+UBSan, and `RUST_DECODE=1 scripts/core-fuzz.sh` fuzzes it.
   `scripts/tessera-decode-lib.sh` prints the link line.
3. **The kernel module.** `kmod/tessera_decode.rs` is a crate root that pulls the same
   source files in with `#[path]`, built by freebsd-kpi-rs's `kmod.rust.mk` (`RUST_SRCS`),
   as in the Rust kmod spike. `kmod/Makefile` adds `-DTESSERA_RUST_DECODE` unless
   `TESSERA_C_DECODE` is set.

## Differential testing

`scripts/tessera-decode-diff.sh` builds one binary that holds two complete cores: the C one,
and the Rust-decoder one with its global symbols renamed `rsmode_*` (`objcopy
--redefine-syms`). `fuzz/diff_body.c` is compiled once per side and records a trace of every
result; `fuzz/fuzz_diff.c` compares the traces and aborts on any difference. Targets: codec,
manifest, pack, btree, journal, superblock. Both sides run under ASAN+UBSan.

- With no arguments it replays every committed corpus through both sides. All agree.
- `-t secs [-j N]` explores with libFuzzer. The Rust side is built with SanitizerCoverage
  (`FUZZ_COVERAGE=1`), so its branches steer the fuzzer too.
- The harness stamps valid magic and CRCs into a share of the inputs, or the fuzzer would
  never get past the first check.

Parity rules the Rust keeps so the traces can match: the binary searches probe in C's
order; the B-tree search starts from `(int)entry_count - 1`; the journal sector count is
computed in 64 bits (C wrapped at 32, see below).

Planted-bug check (2026-11-10): one changed comparison at a time in `btree`, `pack`,
`journal` and `manifest`. The first round caught only the manifest one, which exposed two
holes in the harness, both fixed:

- the B-tree target stamped the leaf value size on every node, so every inner node failed the
  geometry check on both sides and the inner fan-out limit was never compared;
- the script piped each replay into `tail`, so a difference found in replay still exited 0.

After the fixes the B-tree mutant is caught by the corpus replay alone. The pack mutant (`<`
to `<=` on a blob that ends exactly at the end of the data region) and the journal mutant (a
check skipped for one body length) were still missed by 300 s of fuzzing. Both are now caught
by exhaustive unit tests (`pack::boundary_tests`, `journal::tests::block_count_sweep`): exact
boundaries and single-value holes are what sweeps catch and random inputs do not.

## Results (2026-11-10)

- Host: core suite 36/36 with C decoders and 36/36 with Rust decoders (plain and ASAN+UBSan),
  corpus replays clean, differential replay agrees on every corpus, libFuzzer exploration
  clean.
- Kernel: `tessera_fs.ko` (Rust decoders) and `tessera_fs-c.ko` built for arm64
  ATRIUM-NODEBUG. `scripts/tessera-decode-vm-e2e.sh` in the cloud VM (TCG): a 2,500-file
  directory (2,142 entries after removes and renames, so the directory B-tree has inner
  nodes), a 40 MiB file, symlinks, remount, sha256 of every file, fsck clean. Passed 33 of 33
  runs on Rust, C and alternating loads within one boot. Two earlier runs panicked in kernel
  `malloc` (heap corruption), both in boots where the module files had just been overwritten
  on the guest; not reproduced in 33 runs since.
- KASAN (ATRIUM-KASAN kernel, same workload): found an out-of-bounds read in the C journal
  code, not in the decoders. `tessera_journal_tx_begin` copied a fixed 16 bytes from its
  `char[16]` reason tag, and the kmod passes short literals (`"log_drain"`, 10 bytes). It
  hit both modules. Fixed (copy up to the NUL), with `test_short_reason_tag` in
  `core/tests/test_journal.c`, which fails under ASAN on the old code; the existing tests all
  passed 16-character tags, which is why it was never seen.
  A second KASAN run (with the fix) reported nothing, but `cp` hung: a timer interrupt
  arrived while the thread was deep in the write path inside `blake3_hash_many_neon`, and
  KASAN's larger frames pushed the interrupt past the end of the 24 KiB KASAN kernel stack (the
  #114 failure mode: arm64 faults in a loop instead of panicking). That is the C write path
  and the stack-depth item T2, not the decoders. Fixed 2026-11-11: the cause was a
  flush nested inside the write path; see kmod-audit T2.
- Cost: B-tree lookups through the core, host x86_64, callgrind, 100k lookups in a
  20k-entry blob-index tree. First cut: 4,765M instructions against C's 4,557M (about 5%
  more). Causes: per-probe bounds arithmetic in the node search, a byte-at-a-time key
  compare, the node header CRC computed twice per node (once in the C glue, once in Rust),
  and no inlining across the C/Rust boundary. After the fixes (keys compared in 8-byte
  big-endian words, the search loop's bounds math simplified, one CRC per node, and
  `load_node_h` handing the decoded header to the lookup instead of decoding it again):
  4,439M against C's 4,354M (the C side gained from the shared glue fix too), about 2%. The
  rest is the call boundary; cross-language LTO is the remaining lever. Wall time within
  noise on a loaded host. Kernel timing on HVF is a follow-up.
- Rerun after the perf and bug fixes (2026-11-10): host suite 36/36 both ways, differential
  replay and 120 s exploration clean, cloud VM e2e PASS on both modules.

## Bugs found on the way

| where | what | status |
|-------|------|--------|
| `pack.c` `bloom_test` | probe count comes from the header (`bloom_hash_count`), up to 2^32 loops per lookup: a hostile image stalls the reader | fixed: both cap at 64 probes (the builder writes at most 16); `test_bloom_probe_cap` |
| `kmod/tessera_fs.c` directory B-tree | leaf insert/remove walked `count` entries without a bounds check; `cap = count + 1` could overflow; an inner node with `count == 0` indexed `count - 1` | fixed: node blobs go through the Rust `dir_btree_node` check first |
| `kmod/tessera_fs.c` `tessera_fs_dir_btree_walk` | recursion depth came from the image | fixed: bounded at 32 levels |
| `kmod/tessera_fs.c` dir-btree insert and remove | recursion depth came from the image | fixed: bounded at 32 levels, as the walk |
| `journal.c` | `sectors_for` in 32 bits wraps for a huge `body_length`; `journal_open` did not check `head_block`/`tail_block`; a huge body length could make the kernel `malloc(M_WAITOK)` up to 4 GB | fixed: open rejects head or tail outside 1..length-1; record bodies capped at 1 MiB on append and replay (C and Rust), which also keeps the 32-bit sector count from wrapping; `test_bad_head_tail_and_body_cap` |

## Phase 2: the kernel module's pack readers (2026-11-11)

The module reads packs two ways that `tessera_pack_open` cannot serve, because they do not
hold the whole pack: the whole-pack cache (`tessera_fs_pack_extract_blob`, a RAM copy) and
the on-demand reader (`tessera_fs_pack_fetch_ondemand`, header sector, then index, then one
blob). Both parsed the header and index inline, and both had bugs:

- the cache reader trusted the header because the pack had been CRC-checked into the cache.
  A CRC proves the bytes are the ones written, not that they are sane: an index entry with
  `data_offset` near 2^64 made `offset + descriptor + size` wrap, pass the `> packlen`
  check, and `memcpy` from outside the pack;
- the on-demand reader allocated the blob from the entry's `data_size` (up to 4 GiB,
  `M_WAITOK`) before checking it fit, and never bounded the index by the pack's size.

Two entry points in `pack.h` replace that code: `tessera_pack_geom_from_header` (`open`'s
containment rules over the header sector alone, against the bytes the extents hold) and
`tessera_pack_index_find` (binary search of a separately read index; returns a descriptor
offset and size already checked against the data region). Rust in the default build, with a
C reference, both in the differential harness (`run_pack_partial`). A planted off-by-one in
the entry bound was missed by replay and fuzzing, so `pack::span_tests` and
`test_partial_read_api` sweep it exhaustively. The on-demand reader also allocates blobs over
64 MiB without sleeping, since the pack's size still comes from its extent list.

## Phase 2: the userspace tools (2026-11-11)

The Rust tools (`mkfs-tessera`, `tessera-fsck`, `tessera-stat`, ...) link the C core through
`tessera-sys`. It now always links `tessera-decode` (with `ffi`) and, on hosts where it
compiles the core itself, builds it with `-DTESSERA_RUST_DECODE`; `TESSERA_C_DECODE=1` gives
the all-C core. For the guest, `scripts/tessera-tools-cross.sh` compiles the core archive
from `core/Makefile`'s `SRCS` against the cloud sysroot, the same way, and cross-builds the
tools (on the Mac, `bootstrap-atrium.sh`'s corelib phase still builds the archive with bmake;
add `-DTESSERA_RUST_DECODE` there when the Mac flow is next run).

Checked: the property tests (`tessera-property-tests`, all scenarios, with either core); the
cloud VM e2e with the new mkfs and fsck on both kernel modules; and C-built against
Rust-built `tessera-fsck` on the same image, clean and with 60 single-byte flips in the first
64 MiB: identical output and exit status every time (8 of the flips were detected, both
tools reporting them the same way; the rest landed in unused space).

Fixed on the way: the tools did not link on Linux hosts (`tessera-sys` now links libmd there
too), and the property tests had stopped compiling (block-io and format-options fields added
since) and, once compiled, used volumes below the format minimum.

## Not moved yet (phase 2)

- The kernel module's GC index enumerator (`tessera_fs_gc_pack_index_hashes`) keeps its
  own subtraction-style checks; the module's other pack readers and all manifest parsing
  now go through the crate (above).
- The encoders and the write paths (btree insert/split, journal write, pack write): next.
- The loader (`libsa`) stays C: no Rust in the boot loader for now.
- HVF timing on the Mac VM; cross-language LTO.

## How to run

```sh
sh scripts/tessera-decode-check.sh          # cargo test, no-panic and frame check
RUST_DECODE=1 sh scripts/core-host-tests.sh # core suite with Rust decoders
sh scripts/tessera-decode-diff.sh           # C vs Rust on every corpus
sh scripts/tessera-decode-diff.sh -t 600 -j 2
sh scripts/tessera-tools-cross.sh          # guest tools (mkfs, fsck) with Rust decoders
(cd atrium-tessera/rs && cargo run --release -p tessera-test-harness --bin tessera-property-tests)
```
