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
| V6 | atrium-kmod | HOST_BLOB BOs use the BAR offset as `mmap_offset`, ordinary BOs `handle << 30`; the two spaces can overlap within one fd | own fd only | maps the wrong one of the caller's own BOs | open (ABI change; low) |
| **G1** | atrium-gpu-amd | two concurrent `VM_BIND` calls on one BO both passed the "slot free" check and the second wrote `bindings[MAX]`, one past the array | any GPU fd holder | kernel heap overflow | **fixed**: per-BO `bind_lock` (sx) |
| G2 | atrium-gpu-amd | no per-client BO accounting: one client can allocate VRAM until others fail | any GPU fd holder | DoS | open (medium; goes with the scheduler work) |
| G3 | atrium-gpu-amd | QUERY_CAPS fills a 256-byte buffer with no static size check | none today | future overflow if the caps struct grows | open (low; add `_Static_assert`) |
| **T1** | atrium-tessera | QUOTA_SET, GC, DEDUP_POLICY, QUOTA_DUMP and QUOTA_DETACH had no privilege check: any process able to open a directory on a Tessera mount could lift its own quota, re-enable global dedup (the cross-user content oracle the dedup policy exists to close) or start GC | any local user, any jail | quota bypass, information leak, DoS | **fixed**: `!jailed()` and `PRIV_VFS_ADMIN` |
| **Z1** | atrium-zram | `kern.zram.bench` and `kern.zram.selftest` were `CTLFLAG_RD` handlers anyone could read. The bench decompresses live swapped-out pages of every process | any local user, any jail | DoS, compressed-size side channel on other processes' memory | **fixed**: unjailed root only (`PRIV_SYSCTL_DEBUG`) |
| C1 | carillon-kmod | `CARILLON_WAIT` converted `timeout_ms` to ticks in an `int`; above ~24.8 days at hz=1000 it went negative | root (node is 0600) | odd sleep timing | **fixed**: clamp |
| J1 | atrium-tessera core | `write_record` computed `body + in_first` with `body == NULL` and length 0: undefined behaviour in C (UBSAN), harmless in practice since nothing is read | fuzzer input | none observed | **fixed** |
| T2 | atrium-tessera | stack frames over the 1 KiB warning: `tessera_decode_pack_extent_list` 4160 bytes, `tessera_fs_gc_data_zone_ex` 2896, `tessera_blake3_256` 2000, `tessera_vop_readdir` 1808. The kernel stack is 16 KiB on arm64 and a VFS call chain already uses a good part of it | normal use | kernel stack overflow under deep call chains | open (medium; move the buffers to the heap) |
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

- T2 stack frames: worth doing before the Tessera decoders move to Rust (D3), since the Rust
  port would otherwise copy the same layout.
- G2 per-client accounting, G3 static assert, V6 offset spaces: small, not urgent.
- `scripts/core-fuzz.sh`: the `UBSAN_OPTIONS=... \` line is followed by a comment, so the
  variable never reaches the command. Pre-existing; fix with the next fuzz change.
- Syzkaller descriptions for the GPU and Tessera ioctls, after the spike run proves the setup.
