# Audit: Atrium's own kernel modules (step 1)

**Status:** 2026-11-07. Step 1 of [2026-11-07-freebsd-kernel-security-rust.md](2026-11-07-freebsd-kernel-security-rust.md)
(D8). Hand and AI-assisted review of the user-reachable entry points of every Atrium kmod,
plus ASAN fuzzing of the Tessera core parsers. Fixes for every finding rated medium or
higher are in the same commit. Kernel-side runtime checks (KASAN, syzkaller) are in the
spike report, `kmod-rust-spike/README.md`.

**Scope:** atrium-gpu-amd, atrium-kmod (`atrium_virtio_gpu.c`, bootfb, trace),
atrium-tessera kmod (`tessera_fs.c`, 32 k lines) and core, atrium-zram, fresco-kmod,
carillon-kmod. "Reachable by" means who can trigger it on a default Atrium install, given
`etc/atrium.devfs.rules` (which makes `atrium-gpu0` 0666 inside the frescod and GPU jails).

## 1. Findings

| id | module | what | reachable by | impact | status |
|----|--------|------|--------------|--------|--------|
| **V1** | atrium-kmod (virtio-gpu) | `d_mmap` decoded the BO from the offset alone (`handle << 30`, handles are sequential), so any fd on `/dev/atrium-gpu0` could map any BO, including frescod's scanout buffers and other apps' imported surfaces. The token scheme of IMPORT_REGION was only enforced for FREE, not for mmap | any process in a jail that shows `atrium-gpu0` | read and write of other clients' GPU memory | **fixed**: `d_mmap_single`, resolved against the caller's per-fd acl |
| **V2** | atrium-kmod | the device pager caches physical pages. After the last `IOC_GPU_FREE` the BO's `contigmalloc` memory went back to the kernel while user space still had it mapped | same | **use-after-free of physical memory from user space: kernel compromise** | **fixed**: one device-pager object per BO; the object holds a BO reference that only its destructor drops |
| **V3** | atrium-kmod | SUBMIT, RESOURCE_ATTACH, SET_MODE and PAGE_FLIP looked the BO up under `bo_lock`, dropped the lock and kept using it; a concurrent FREE freed it underneath (SUBMIT's comment called this "UB; the server must serialise") | any GPU fd holder, against itself | kernel use-after-free | **fixed**: `atrium_bo_hold_locked` / `atrium_bo_drop` around each use |
| **V4** | atrium-kmod | `atrium_bo_free` reached the softc through `bo->owner`, the minting fd, which is freed when the minter closes while an importer keeps the BO | minter closes before importer | kernel use-after-free on BO free | **fixed**: `bo->sc` |
| **V5** | atrium-kmod | FREE and MINT_TOKEN walked and edited the per-fd acl without `bo_lock`; two threads on one fd raced | any GPU fd holder | list corruption, use-after-free | **fixed**: acl edits under `bo_lock` |
| **V6** | atrium-kmod | HOST_BLOB BOs use the BAR offset as `mmap_offset`, ordinary BOs `handle << 30`; the two spaces can overlap within one fd | own fd only | maps the wrong one of the caller's own BOs | **fixed** 2026-11-11: host-visible BOs get their own range (`ATRIUM_HOST_MMAP_BASE` 2^62 + BAR offset), disjoint from `handle << 30` by a static assert. Offsets stay opaque to userspace, so no ABI change in practice; builds clean, not yet run on the Mac VM's host-blob path |
| **G1** | atrium-gpu-amd | two concurrent `VM_BIND` calls on one BO both passed the "slot free" check and the second wrote `bindings[MAX]`, one past the array | any GPU fd holder | kernel heap overflow | **fixed**: per-BO `bind_lock` (sx) |
| G2 | atrium-gpu-amd | no per-client BO accounting: one client can allocate VRAM until others fail | any GPU fd holder | DoS | open (medium; goes with the scheduler work) |
| G3 | atrium-gpu-amd | QUERY_CAPS fills a 256-byte buffer with no static size check | none today | future overflow if the caps struct grows | **fixed** (2026-11-11): QUERY_CAPS is built in Rust (`rs/ioctl.rs`, `Caps::put`), which bounds-checks every record against the buffer and fails with ENOMEM instead of overrunning |
| **T1** | atrium-tessera | QUOTA_SET, GC, DEDUP_POLICY, QUOTA_DUMP and QUOTA_DETACH had no privilege check: any process able to open a directory on a Tessera mount could lift its own quota, re-enable global dedup (the cross-user content oracle the dedup policy exists to close) or start GC | any local user, any jail | quota bypass, information leak, DoS | **fixed**: `!jailed()` and `PRIV_VFS_ADMIN` |
| **Z1** | atrium-zram | `kern.zram.bench` and `kern.zram.selftest` were `CTLFLAG_RD` handlers anyone could read. The bench decompresses live swapped-out pages of every process | any local user, any jail | DoS, compressed-size side channel on other processes' memory | **fixed**: unjailed root only (`PRIV_SYSCTL_DEBUG`) |
| C1 | carillon-kmod | `CARILLON_WAIT` converted `timeout_ms` to ticks in an `int`; above ~24.8 days at hz=1000 it went negative | root (node is 0600) | odd sleep timing | **fixed**: clamp |
| J1 | atrium-tessera core | `write_record` computed `body + in_first` with `body == NULL` and length 0: undefined behaviour in C (UBSAN), harmless in practice since nothing is read | fuzzer input | none observed | **fixed** |
| T2 | atrium-tessera | stack frames over the 1 KiB warning: `tessera_decode_pack_extent_list` 4160 bytes, `tessera_fs_gc_data_zone_ex` 2896, `tessera_blake3_256` 2000, `tessera_vop_readdir` 1808. The kernel stack is 16 KiB on arm64 and a VFS call chain already uses a good part of it | normal use | kernel stack overflow under deep call chains | **fixed** 2026-11-11: the pack extent list decoder is Rust now (D3, 112 bytes); the BLAKE3 hasher and the GC walk stack moved to the heap (`gc_data_zone_ex` 2912 to 1248, `blake3_256` 2000 to 48); `vop_readdir` 1808 to 1632. The real cause of the KASAN overflow was a nested flush, see below |
| F1 | fresco-kmod, carillon-kmod | any opener can map the whole ivshmem region | root (0600, dev transport only) | none on a production install | open (by design: VM transport) |

The two most serious bugs (V1, V2) are in the virtio-gpu driver that the Mac VM uses for
Fresco today. They are the textbook `d_mmap` mistakes, and FreeBSD's own drivers made them
too before `d_mmap_single` existed. atrium-gpu-amd already used `d_mmap_single` with a
managed pager and revocation, which is what the fix copies.

## 2. What was checked and how

- **Every ioctl, read, write, mmap and sysctl handler** of every module was read for: length
  and index checks on user input, integer overflow in size arithmetic, use of an object after
  its lock is dropped, refcount balance on every error path, privilege checks, and uninitialised
  bytes copied out.
- **Tessera** (32 k lines): the ioctl handler, extended attributes (set/get/list/delete),
  readdir and the FIOSEEK paths were reviewed by hand. The on-disk decoders are covered by
  the core fuzzers (below). The VFS operations as a whole are left to syzkaller on a Tessera
  mount; a full line-by-line review is out of reach in one pass.
- **Tessera core fuzzing:** `scripts/core-fuzz.sh` now builds with ASAN on Linux
  (`FUZZ_ASAN=1`). Results are in §3.

## 3. Fuzzing results (Tessera core, libFuzzer + ASAN + UBSAN, Linux host)

| target | time | execs | result |
|--------|------|-------|--------|
| fuzz_manifest | 600 s | 11.1 M | clean |
| fuzz_pack | 600 s | 40.2 M | clean |
| fuzz_btree | 600 s | 7.6 M | clean |
| fuzz_superblock | 600 s | 8.8 M | clean |
| fuzz_journal | 600 s + 60 s | 0.15 M | **J1**, then clean after the fix |

Every target's corpus was then replayed under ASAN + UBSAN: clean. The corpus growth (22 MB)
is not committed; the J1 input is, as `fuzz/corpus/fuzz_journal/regress-2026-11-07-null-body`.

## 4. Follow-ups

- T2 stack frames: the decoder frame went with D3 (the Rust decoders read fields in place;
  largest frame 112 bytes, checked by `scripts/tessera-decode-check.sh`). The other three
  frames are still open.
- Found during D3 (2026-11-10, [rust-decoders.md](../../atrium-tessera/docs/rust-decoders.md)):
  the C pack reader's bloom probe count comes from the image (up to 2^32 loops per lookup;
  fixed, capped at 64 like the Rust); the kmod's directory B-tree insert and remove walked
  `count` entries unchecked and an empty inner node indexed `count - 1` (fixed by the Rust
  node check); directory B-tree walk, insert and remove recursion came from the image (all
  three fixed, 32 levels); `journal_open` did not check `head_block`/`tail_block`, and a huge
  record body length could make the kernel `malloc(M_WAITOK)` up to 4 GB (both fixed: open
  rejects an out-of-ring head or tail, record bodies are capped at 1 MiB on write and read).
  Each fix has a host test that fails without it.
- KASAN run of the Tessera e2e workload (2026-11-10, first time Tessera ran under KASAN):
  `tessera_journal_tx_begin` read past its short reason-tag literal (16-byte copy of
  `"log_drain"`). Fixed, with a host regression test. A second KASAN run hung `cp` on a kernel stack
  overflow: a timer interrupt landed while the write path was deep in
  `blake3_hash_many_neon` (T2, #114). Fixed 2026-11-11. `scripts/kmod-stack-depth.py`
  (static: frames from the prologues, deepest direct-call chain) found why: `mark_dirty`
  runs a synchronous flush, and it is called from deep inside the publish path that a flush
  itself runs (publish, `dead_extent_record`, `mark_dirty`, flush), so a write could nest a
  whole second flush pipeline. `vop_write`'s deepest chain in the module alone was 8 KiB on
  NODEBUG (16 KiB stack) and 22.5 KiB on KASAN (24 KiB stack). Now the inline flush runs only
  when enough stack is left (`GET_STACK_USAGE`, as `geom_io` does for direct dispatch: 8 KiB
  NODEBUG, 18 KiB KASAN, the flush chain being 5.7 and 14.5 KiB); deeper, it goes to the
  flush taskqueue (`kern.tessera.flush_stack_deferred` counts these;
  `kern.tessera.flush_stack_need` sets the bound, and a huge value forces the queued path for
  testing). With the two heap moves, the deepest chain is 6.2 KiB NODEBUG and 15.7 KiB KASAN.
  The e2e workload under KASAN (`KERNCONF=ATRIUM-KASAN scripts/tessera-decode-vm-e2e.sh`) now
  passes on both modules with no KASAN reports, also with `SYSCTLS=` dropping the flush
  thresholds to 1 (a flush every few mutations) and with `flush_stack_need` huge. No flush
  was deferred in any run, because the async threshold flush keeps `dirty_count` under the
  synchronous backstop: the nested inline flush is rare, and the hasher move is what took
  the chain that hung off the stack's end. The queued path is the same `taskqueue_enqueue`
  the async threshold already uses on every run.
- D3 phase 2 (2026-11-11): the kmod's whole-pack cache reader could read outside the pack
  (an index `data_offset` near 2^64 wrapped the bounds check), and its on-demand reader
  allocated a blob size from disk (up to 4 GiB, `M_WAITOK`) before checking it. Both now
  use the checked Rust partial-read API (`tessera_pack_geom_from_header`,
  `tessera_pack_index_find`); see rust-decoders.md.
- G2 per-client accounting and G3 caps size check: taken by the GPU driver Rust port (G3 closed there by the bounds-checked Rust QUERY_CAPS). V6 fixed 2026-11-11.
- `scripts/core-fuzz.sh`: the `UBSAN_OPTIONS=... \` line was followed by a comment, so the
  variable never reached the command. Fixed 2026-11-10 with the D3 fuzz changes.
- Syzkaller descriptions for the GPU and Tessera ioctls, after the spike run proves the setup.
