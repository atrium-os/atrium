# atrium-zram v2: hardening the compressed-RAM tier

> **Status (2026-11-03):** increment 1 (counters, §4.6) is **built and
> verified in the FreeBSD VM**, together with BIO_DELETE support and a selftest
> fix. The first FreeBSD run (§3.1) found a v1 problem bigger than any in the
> original list: **pages freed by the VM stay in the store** (§1 item 7).
> **Increment 0 (§4.0) fixes it in the kernel** (atrium-os/freebsd-src
> `da0b30649`), verified in-VM the same day. The §3 numbers come from a Linux host
> used as a stand-in. §3.1 covers what has been confirmed on FreeBSD so far.
>
> **2026-11-04:** increment 2 (packed allocator, memory limit, reserve; §4.2,
> §4.4) is **built and verified in-VM**. On the same 1.9 GB workload the
> store's memory overhead over the compressed data fell from **40 % to 3.9 %**,
> failed page-outs fell from 356 to 0, and no pages were corrupted. The limit
> holds exactly (§4.4).
>
> **2026-11-04 (later):** increment 3 (per-CPU codec contexts, hashed slot
> locks; §4.1, §4.3) is **built and verified in-VM**. Four parallel `dd` writers
> run 2.2× faster than on increment 2 (3.96 s vs 8.84 s for 128 MiB), and the
> 1.9 GB fill still has 0 corrupt pages. The extra concurrency exposed a weak
> spot in increment 2's span-header reserve, now replaced by the pool's own
> spare-header list (§4.3 "As built").
>
> **2026-11-04 (later still):** increment 4 (device size tunable, lazily
> allocated slot table; §4.1) is **built and verified in-VM**. The module's
> memory at load fell from 12.0 MiB to 99 KiB, a 4 GiB device works on a 2 GiB
> VM, and the 1.9 GB fill still has 0 corrupt pages and 0 failed stores.
> Next: increment 5 (backing store and writeback).
>
> **Decision context (Shankar, 2026-11-03).** Atrium gets macOS-style compressed
> main memory, and it must work in the Mac VM, which has no GPU. Using spare GPU
> VRAM as RAM overflow was **rejected**: a GPU reset wipes VRAM, games reclaim it,
> and holding it keeps a laptop dGPU from powering down. The direction chosen was
> **harden the existing zram device first, then decide** whether a VM-native
> compressor (the macOS shape) is worth its cost (§6).
>
> Parent spec: [`atrium-memory-pressure.md`](atrium-memory-pressure.md) (Phase 3
> is the compressed tier; §9.5 covers deployment).

## 1. What exists (v1)

`atrium-zram/atrium_zram.ko` creates `/dev/zram0`, a `disk(9)` block device that
FreeBSD swaps into. Each 4 KiB page written becomes one slot in a fixed array:

| state | stored as | cost |
|---|---|---|
| SAME | the fill byte (all-zero or all-one-byte page) | 0 bytes |
| COMP | zstd level 3 output, in its own `malloc(9)` buffer | the malloc zone size |
| RAW  | the page verbatim, if zstd didn't shrink it | 4096 |

The codec is the kernel's in-tree zstd, with contexts allocated in advance at
load. It has been verified end to end: it works as a `dd` block device, as live
swap under a 12 GB hog, and it auto-starts at boot (rc.d, deferred until after
sshd). The rest of the memory stack uses it without wiring to it. PSI `full`
falls when zram absorbs pressure, so `memoryd` stays dormant (invariant §7.6 of
the parent spec).

**Known limits of v1** (from the source, `atrium_zram.c`):

1. **One global mutex** covers both codec contexts and the whole slot table,
   so every page-in and page-out on every CPU runs serially.
2. **One `malloc(9)` per page.** This wastes bytes on rounding (§3), and the
   `M_NOWAIT` allocation can fail exactly when the pageout daemon needs it to
   succeed. A failed store fails the swap write. *(Fixed by increment 2.)*
3. **Fixed size.** It is 2 GiB of swap (`ZRAM_NSLOTS`), and the 12 MiB slot
   table is wired at load whether or not it is used. *(Fixed by increment 4.)*
4. **No memory limit and no writeback.** Nothing bounds how much RAM the store
   takes, and incompressible or cold pages have nowhere to go but RAM.
5. **Tiering through swap is wrong on FreeBSD.** The swap allocator has no
   priorities and stripes across every active device. With both `/dev/zram0`
   and a disk partition swapped on, about half the evicted pages go straight to
   disk.
6. **Thin observability.** Only `kern.zram.phys_bytes` is exposed. There is no
   logical size, no page counts by state, and no failure count. *(Fixed by
   increment 1.)*
7. **Freed swap is never released (found in-VM, §3.1).** FreeBSD's swap pager
   frees a swap block without telling the device. Linux has
   `swap_slot_free_notify` for this, and FreeBSD has no equivalent. So a page
   stays compressed in RAM after its process exits, until something reuses its
   block. Measured: after a 1.9 GB workload exited, `swapinfo` showed 107 MB in
   use while the store still held 1.7 GB logical in **0.96 GB of kernel
   memory**. On a long-running desktop this is the dominant cost of the tier.
   `swapon -E` does not help: it trims the whole device once, at swapon.
   *(Fixed by increment 0, §4.0.)*
8. **The selftest used live slots 0–2.** With the device swapped on, slot 2 can
   hold a real swapped-out page (the swap pager reserves only the first
   `BBSIZE`), so running the selftest could corrupt a process's memory. *(Fixed:
   it now uses private slots.)*

## 2. What macOS does that matters, and what doesn't

The macOS compressor lives inside the VM's pageout path. It packs compressed
pages into fixed-size segments and, under further pressure, writes whole cold
segments to disk sequentially. Taken apart:

- **Packed segments** remove allocator waste and fragmentation. *Matters a lot;
  doable inside zram (increment 2).*
- **An integrated compressed → disk hierarchy** (cold data leaves RAM in large
  sequential writes). *Matters on any machine with disk swap; doable inside zram
  by giving zram its own backing store (increment 5).*
- **Per-CPU compression.** *Doable inside zram (increment 3).*
- **No block-layer hop** (pageout → `swap_pager` → bio → GEOM → `d_strategy`).
  *Probably small:* a few µs of bookkeeping next to 6–20 µs of compression
  (§3). It has to be measured in-kernel before it justifies forking the core VM
  (§6).

## 3. Measurements (memprobe, Linux host stand-in, 2026-11-03)

`memprobe` (new, repo root) reads the resident anonymous pages of live
processes. Those are exactly what pageout would hand to swap. For each page it
records what the zram store would do with it, under LZ4 and zstd (levels 1 and
3), and what each stored page would really cost under three allocators:
*ideal* (exact bytes), *packed* (32-byte classes, the zsmalloc / macOS-segment
shape), and *malloc9* (the FreeBSD zones v1 uses). The ratios below use the
packed allocator unless the column says otherwise.

| workload | anon read | zero % | dup % | LZ4 | zstd-3 | zstd-3, malloc9 (= v1 today) |
|---|---|---|---|---|---|---|
| 30 Navigator workers, one document each (Go spec, npm docs) | 24 MB | 0.5 | 2.9 | 2.83× | 4.68× | 3.47× |
| `cargo build --release` of navigator-normalize, 4 snapshots of rustc | 57–357 MB | 0.2–3.0 | 0.1–0.3 | 2.60–2.83× | 4.04–4.52× | 2.77–3.13× |
| 8 Chromium tabs (local docs pages; browser-tab stand-in) | 202 MB | 12.4 (+5.5 same-filled) | 14.5 | 4.10× (3.37× excl. same-filled) | 6.46× (5.31×) | 4.57× |

Per-page timing (user-space Rust, `lz4_flex` safe mode and the `zstd` crate, on
the cloud x86 CPU; useful for comparing codecs, not as in-kernel numbers):
LZ4 compress 6–10 µs and decompress 3–4 µs; zstd-3 compress 15–23 µs and
decompress 6–7 µs. RAW (incompressible) pages: ≤0.4 % with LZ4 and ~0 % with
zstd.

**What the numbers say:**

1. **The v1 headline (~175×) doesn't describe real memory.** It came from a hog
   whose pages were almost all zero. Pages that carry data compress about 2.6–3×
   with LZ4 and about 4–5× with zstd. Zero pages are a big win only for the
   browser (12–18 % of pages).
2. **The allocator is the largest fixable loss.** `malloc(9)` rounding wastes
   27–35 % of the store, so a packed allocator recovers roughly 1.35–1.5× capacity
   with no codec change. This is increment 2, and the biggest one.
3. **zstd beats LZ4 on capacity by about 1.6×**, at about 2× the decompress
   cost. Both are microseconds, against 100 µs or more for an SSD read. That
   **reverses an earlier assumption** that LZ4 should be the default: keep zstd
   as the default and offer LZ4 as a latency/energy option (increment 6). Revisit
   once in-kernel timings exist.
4. **Dedup** is worth about 15 % for the browser and near nothing for builds or
   Navigator workers. It is deferred (§8).
5. **The packed allocator's size classes:** compressed sizes spread fairly evenly
   from 256 bytes to 2.5 KiB, with almost nothing near 4 KiB. Uniform 32-byte
   classes are enough, and the "huge object" threshold can sit at ~3 KiB.

Caveats: these are Linux processes on x86, and the Navigator documents are local
docs pages, not the real corpus. FreeBSD's jemalloc and the aarch64 build may
differ. §7 repeats the measurements in the VM.

### 3.1 First FreeBSD run (cloud VM, 2026-11-03)

Setup: stock FreeBSD 16.0-CURRENT arm64 under QEMU TCG (`scripts/cloud-vm.sh`)
with 2 GB RAM, so it has no Atrium kernel and no PSI. The v1 kmod plus
increment 1 was built in the guest with `-Werror` and loaded, the disk swap was
turned off, and `/dev/zram0` was the only swap device. The workload was 30
cross-built `navigator-worker`s each holding one document, plus `fill` (`atrium-zram/test/fill.c`, built in the guest with `cc`): 1.9 GB
of real file contents (binaries, libraries, text from `/usr` and
`/boot/kernel`) held in anonymous memory with a 10 % hot set, for 300 s. It was
recorded with `memprobe watch`.

- **Counters agree with the kernel.** `orig_bytes` was 1014.5 MiB while
  `swapinfo` showed 1013 MiB, and the selftest passed with swap live. A `dd`
  round-trip of 200 random and 50 zero pages matched, and `trim(8)` of 100 pages
  freed exactly 100 slots.
- **Allocator waste confirmed.** At peak the store held 1354 MiB logical in 518
  MiB of compressed data, but `vmstat -m` showed **727 MiB** of `malloc(9)`
  memory (excluding the 12 MiB slot table). Rounding cost **29 %** of the
  footprint, in line with the 27–35 % predicted in §3. The effective ratio is
  2.61× on data but **1.95×** on real memory. The kernel's histogram shows why:
  about half the compressed pages landed at 1–2.5 KiB, where zones round up to 2048 or 4096.
- **`M_NOWAIT` failures are real.** `fail_nomem` = 356: under pressure, 356
  swap-outs failed because `malloc` had nothing to give. Increment 2's reserve
  (§4.2) is required, not just nice to have.
- **Freed swap stays resident** (§1 item 7). After the workload exited, the
  store still held 1696 MiB logical / 961 MB of kernel memory against 107 MB of
  swap in use, and `deletes` stayed at 0.
- **Navigator workers on FreeBSD:** jemalloc leaves most heap regions partly
  resident, so the probe needs `--fault-partial` (§7). 42.5 MB resident anon
  across 30 workers: 6.7 % zero, **35.7 % same-filled** with a non-zero byte, and
  4.9× LZ4 on the rest. The same-filled share is almost certainly -CURRENT's
  debug malloc junk-filling freed memory. A release build (`MALLOC_PRODUCTION`)
  won't show it, so don't plan around it.
- TCG makes timings meaningless here (memprobe reported ~80 µs per LZ4 page),
  so per-page cost still needs the Mac VM (HVF) or hardware.

## 4. Design

### 4.0 Swap-free notification (fixes §1 item 7; kernel) — DONE

A leaf module cannot see a swap block being freed, so this is the one piece that
needs a kernel change, on the atrium-os `freebsd-src` branch alongside
`kern_pressure.c`. In `swap_pager.c`, when `swp_pager_freeswapspace` returns a
range to a device's blist, queue a `BIO_DELETE` for that range to the device's
GEOM consumer. Do it only for devices whose provider reports
`GEOM::candelete`, which zram's `DISKFLAG_CANDELETE` does. Batch the ranges and
send them from a taskqueue, never from the freeing context, which may hold the
object lock. This is the same design as Linux's `swap_slot_free_notify`, done
through FreeBSD's ordinary BIO interface. It also gives disk swap on SSDs online
TRIM, which FreeBSD lacks today.

The kmod side is done: `BIO_DELETE` frees every slot the range covers and
counts it in `kern.zram.deletes`. Verify with `deletes` rising and `orig_bytes`
falling back to `swapinfo`'s figure after a workload exits.

**As built** (atrium-os/freebsd-src `da0b30649`, `sys/vm/swap_pager.c`): the
queue is a **per-device bitmap** (`sw_delmap`, one bit per swap block,
allocated at swapon), not a ring. The first version used a 4096-entry ring.
When the 1.9 GB fill exited it dropped 8914 ranges and left 775 MB behind,
because a process exit frees thousands of scattered ranges at once. Marking a
bit cannot overflow, and neighbouring frees coalesce into one delete. The task
sends runs of up to 1024 pages. `swapoff` waits for marked and in-flight
pages to drain. Knobs: `vm.swap_free_notify` (default 1), with
`vm.swap_free_notify_{sent,pages}` as counters.

**Verified in-VM** (ATRIUM kernel cross-built on the Linux host with
`tools/build/make.py`, 2 GB, zram the only swap). The workload was
`atrium-zram/test/fill.c`: 1.9 GB of real file data, random pages dirtied
throughout so blocks are freed and reused mid-run, and every page checksummed.
Peak swap was 1.9 GB and PSI `some` reached 37.5 %. **0 of 486,400 pages were
corrupt**, 750,819 pages were deleted in 18,023 `BIO_DELETE`s, and after exit
the store held **8.1 MiB, matching `swapinfo`** (it was 1.7 GB before the
fix). Memory returned to the 13 MB slot table. The notification also turned on
for the virtio disk swap partition, which gives SSD-backed swap online TRIM.

### 4.1–4.8: inside the module

Unchanged from v1: it is a `disk(9)` device that the rc.d script swaps on; the
SAME / COMP / RAW states stay; it is TCB-loaded and never apps-loadable; and the
module stays a leaf (no edits to the kernel VM).

### 4.1 Slot table

- Size is set by a tunable, `kern.zram.disksize` (default: physical RAM, so the
  device can hold ~3–4× RAM's worth of logical swap at measured ratios). It is
  read at load.
- The table is allocated in **chunks** of 64 Ki slots on first write to each
  chunk range (`M_NOWAIT` from the reserve, falling back to failing the bio).
  This avoids wiring 12 MiB+ up front.
- A slot shrinks to 8 bytes: a 2-bit state, a 2-bit flag (*idle*, *written
  back*), and a 60-bit handle (a packed-allocator handle, the fill byte, or a
  backing-store block).
- Locking: **hashed slot locks** (256 `mtx`, indexed by slot number) instead of
  one global lock. The bio for a page holds only that page's lock.

**As built (increment 4, 2026-11-04).**

- `kern.zram.disksize` (bytes, loader tunable; `4G` style suffixes work) is
  read at load. The default is physical RAM. It is clamped to 16 MiB–1 TiB and
  rounded up to a whole chunk. The read-only sysctl of the same name reports
  the size in use.
- The table is a directory of chunk pointers. A chunk is **4096 slots
  (64 KiB)**, not 64 Ki: under pressure a 64 KiB `M_NOWAIT` allocation is far
  likelier to succeed than a 1 MiB one, and the directory is still small (32 KiB
  for 1 TiB). A write into an unallocated chunk allocates it `M_NOWAIT`; if
  that fails it takes a **spare chunk** that the refill task replaces; if both
  fail the write fails with ENOMEM (`fail_table`). Two writers racing for a
  chunk publish with a compare-and-swap, and the loser frees its copy. A read of
  a never-written chunk returns zeros, and a delete skips the chunk.
- The slot is **16 bytes** (v1: 24): span pointer, object index, length,
  state, fill byte. The 8-byte packed handle above needs a handle table in the
  pool so a slot can name its span in under 64 bits. That is left for
  increment 5, which has to add a "written back" state to the slot anyway.
- Chunks are not freed when they empty again; they live until unload. Swap
  stripes across the whole device, so a busy device touches most chunks
  anyway (see below), and freeing would need a per-chunk live count on the hot
  path. Deferred (§8).
- New sysctls: `disksize`, `table_bytes`, `table_spare_used`, `fail_table`.
- Measured in the VM (2 GiB RAM):
  - Module malloc at load (`vmstat -m`, type `zram`): **12,617,728 bytes on
    increment 3, 101,376 on increment 4.**
  - Default device: 2,113,929,216 bytes (physical RAM, chunk-rounded).
    Round-trips at the first and last 4 MiB match, and an unwritten region in
    between reads as zeros. 128 KiB of table was allocated for the two writes.
  - With `kern.zram.disksize=4G`: a 4 GiB device; a 4 MiB round-trip at
    3.5 GiB matches with 64 KiB of table allocated.
  - The 1.9 GB fill: 0 of 486,400 pages corrupt; `fail_nomem`, `fail_table`,
    `reserve_used` and `hdr_reserve_used` all 0; the table grew to 7.4 MiB
    (119 of 126 chunks) as swap spread across the device.

### 4.2 Packed allocator (`zpool`)

zsmalloc-shaped, sized from §3:

- **Size classes** in 32-byte steps from 32 bytes to 3072 bytes. A compressed
  object over 3072 bytes is stored RAW (the measured share is ≤0.4 %, so no class
  is worth it).
- **Zone pages**: each class carves objects out of *k*-page spans, choosing *k*
  in 1–4 to minimise tail waste. Spans come from `kmem_malloc` as contiguous KVA,
  so an object can straddle a page boundary with no mapping tricks.
- **Free tracking**: a per-span bitmap, plus per-class lists of partial spans.
  Allocation takes the class lock only.
- **Reclaim-path safety**: allocations use `M_NOWAIT | M_USE_RESERVE`, and the
  pool keeps a small reserve of free spans per class (refilled outside the I/O
  path by a taskqueue). If both fail, the store falls back to writeback (§4.5) if
  configured, and otherwise returns `ENOMEM` on the bio. In that case
  `swap_pager` keeps the page dirty and the system continues.
- **Compaction** (later): sparse spans are migrated into fuller ones when the
  pool's fragmentation passes a threshold. This is the macOS segment-compaction
  analog. It is not needed until the counters show fragmentation.

**As built** (`atrium-zram/zram_pool.{c,h}`, 2026-11-04):

- There are 96 classes, 32 to 3072 bytes, plus a one-page RAW class. Each
  class uses the fewest pages (1–4) that come within 1 % of its best tail
  waste. Compressed pages over 3072 bytes are stored RAW: with spans of at most
  4 pages, a 3.1–4 KiB object costs a page anyway, and RAW skips the
  decompress.
- Span memory comes from `kmem_malloc(M_NOWAIT | M_USE_RESERVE)`. Span headers
  come from a UMA zone with `uma_zone_reserve` and `uma_prealloc`, so a header
  never eats into a span.
- The reserve is 256 chunks of 4 pages (4 MiB; tunable
  `kern.zram.reserve_chunks`). An empty 4-page span goes back to the reserve
  while it is below target. A taskqueue refills it whenever it drops below
  half.
- Counters: `kern.zram.{pool_bytes, obj_bytes, spans, reserve_avail,
  reserve_used, fail_limit}`. `pool_bytes` is the store's real footprint.
  `pool_bytes - obj_bytes` is span fragmentation.
- Userspace test (`atrium-zram/test/zpool_test.c`, the same source under a
  shim): 120k random alloc/free operations on the measured size mix, with a
  pattern written to every object and checked before it is freed. It audits
  the accounting against a recount, injects page-allocator and header failures
  (the reserve absorbs them, then `ENOMEM`), and checks that the limit refuses
  rather than overshoots. Under pure random churn, span fragmentation settles
  near 15 % with 20k live objects. That is the case compaction would address,
  and the real workload below did far better.
- **In-VM** (ATRIUM kernel, 2 GB, zram the only swap, limit off), on the same
  1.9 GB checksummed fill as §4.0, at peak: 1854 MiB logical, 782 MiB of
  compressed data, 788 MiB class-rounded, and a **813 MiB pool**. That is
  **3.9 % overhead over the data, against v1's 40 %** (727 MiB of `malloc` for
  518 MiB of data). `malloc(9)` now holds only the 12 MiB slot table. Results:
  0 of 486,400 pages corrupt, `fail_nomem` = 0 (v1: 356 on the same kind of
  run), the reserve never touched, and an empty pool after exit.

### 4.3 Per-CPU codec contexts

One pre-allocated `ZSTD_CCtx` and `ZSTD_DCtx` per CPU (`mp_ncpus`), each behind
its own mutex. A bio uses `curcpu`'s context. If the thread migrates, it is still
correct because the context is locked, not pinned. There is no `malloc` on the
path, as in v1. The compression scratch buffer becomes per-context too (in v1 it
is a static guarded by the global mutex).

**As built (increment 3, 2026-11-04).**

- `struct zram_ctx` per CPU: a mutex, a `ZSTD_CCtx`, a `ZSTD_DCtx` and a
  4.5 KiB compression buffer, cache-line aligned, all allocated at load.
- 256 hashed slot mutexes (`mtx_padalign`), indexed by the slot's address so
  the selftest's private slots work the same way. This is the §4.1 locking
  item; the rest of §4.1 (sizing, lazy table, 8-byte slot) is increment 4.
- The pool keeps one mutex, held only for `zpool_alloc` / `zpool_free`, never
  across the codec. Lock order is slot → codec context → pool. A load
  decompresses straight from pool memory under the slot lock, which is safe
  because only a holder of the slot lock frees its object and spans never move.
- Counters are `atomic_add_64`; the sysctl readers take no lock.
- Measured in the VM (QEMU TCG, 4 vCPUs, 32 MiB of mostly-incompressible
  `/usr/share` + `/usr/include` text per stream, 64 KiB `dd`):

  | | increment 2 | increment 3 |
  |---|---|---|
  | 1 writer, 32 MiB | 2.88 s | 2.83 s |
  | 4 writers, 128 MiB | 8.84 s | **3.96 s** |
  | 1 reader | 0.82 s | 0.91 s |
  | 4 readers | 2.00 s | 1.71 s |

  All streams read back byte-identical. Under TCG the 4 vCPUs are host
  threads, so the absolute times mean little; the ratio is the point. Reads
  gain less because most of this data is RAW (a `memcpy`, not a decompress).
- **Span-header fix.** The first 1.9 GB fill on increment 3 had 0 corruption,
  but it also had 40 `fail_nomem` (silent ENOMEM page-outs; the pager keeps
  the page and retries), where increment 2 had 0. The chunk reserve was barely
  touched (69 of 256 used), so the failures were span headers. UMA's zone
  reserve does not work as a reserve when every allocation passes
  `M_USE_RESERVE`: steady churn drains it, and the zone then needs a new slab
  `M_NOWAIT` under pressure. The pool now keeps its own list of spare headers
  (`2 × reserve_chunks + 64`), taken only when UMA refuses, topped up by the
  same refill task. New sysctls: `hdr_reserve_avail`, `hdr_reserve_used`. The
  userspace test injects header failures to cover it. Re-run of the fill:
  **0 corrupt, `fail_nomem` 0**, with 181 spare headers and 218 reserve chunks
  used at the peak (PSI `some` 56 %, min free 21 MB), and an 8 MiB store after
  exit matching `swapinfo`.

### 4.4 Memory limit

`kern.zram.mem_limit` (bytes; default 25 % of physical RAM; 0 = unlimited) caps
the physical bytes the store may hold. Over the limit, a write goes to writeback
(§4.5) or fails with `ENOSPC`. That makes swap full, which is what PSI `full` and
`memoryd` already react to. It keeps the compressed tier from consuming the
memory it is meant to save.

**As built and measured (2026-11-04).** In-VM, with the limit set to 300 MiB
under a 1.7 GB fill, the pool peaked at exactly 314,572,800 bytes and refused
17,798 stores (`fail_limit`). The feedback path is crude, though. FreeBSD's
swap pager treats `ENOSPC` as an I/O error: 1,331 `pageout failed` lines went
to the console, the pages stayed dirty, and the kernel OOM killer ended the fill
while `swapinfo` still showed free space. PSI `some` reached 51.7 %, which
`memoryd` would act on first in a real system. Increment 5's writeback turns
"at the limit" into "move cold pages to disk" instead of failing. Until then,
the default (25 % of RAM) is a safety bound, not a tuning knob.

### 4.5 Backing store and writeback (fixes v1 limit 5)

zram optionally owns a backing **vnode or GEOM provider**
(`kern.zram.backing_dev`, set once before the first write). Only `/dev/zram0` is
swapped on, never the disk partition, so FreeBSD's round-robin striping can no
longer put fresh pages on disk.

- **RAW pages** go straight to the backing store (they gain nothing from RAM).
- **Idle pages**: a periodic scan marks every COMP slot idle, and a read clears
  the mark. Slots still idle at the next scan (default period 5 min,
  `kern.zram.idle_secs`) are written back in batches of up to 256 pages, as
  sequential writes. This is the macOS cold-segment write-out.
- Over `mem_limit`, idle slots are written back first, then newest-written.
- The backing store is block-addressed by its own bitmap allocator. A written-back
  slot records the block. A read from a written-back slot reads it
  synchronously, then frees the block, since the page is now resident again in
  the VM.

### 4.6 Observability

Read-only sysctls under `kern.zram`: `orig_bytes` (logical), `phys_bytes`
(compressed data), `pool_bytes` (allocator footprint including waste),
`pages_{same,comp,raw,wb}`, `reads`, `writes`, `fail_{nomem,limit,io}`,
`wb_{writes,reads}`, and `hist` (compressed-size histogram in 256-byte buckets,
like memprobe). `memprobe watch` already reads `orig_bytes` and `phys_bytes`
when present.

### 4.7 Codec selection

`kern.zram.codec` = `zstd` (default) | `lz4`, plus `kern.zram.zstd_level`
(default 1: measured within 3 % of level 3 on capacity). It is set before first
write. LZ4 needs a vendored copy of the BSD-2 reference implementation inside the
kmod, because the kernel's LZ4 lives in ZFS and is not a stable export. Pick the
codec per device, so a future per-tier scheme (LZ4 in front, zstd recompression
of idle pages, like Linux zram's `recompress`) stays possible.

### 4.8 Federation hooks

No new wiring is needed for correctness, since tiers still couple through PSI.
Two small consumers:

- `memoryd` logs zram fill (`phys_bytes / mem_limit`) next to PSI, so a reap
  decision shows whether the compressed tier was already full.
- `memfed` gains an optional `--zram-ratio` input. A jail's swapped-to-zram
  pages cost `1/ratio` of their size, so the budget can count compressed memory
  at its real price. (RCTL's `swapuse` counts logical bytes; this corrects
  for it.)

## 5. Increments (each verified before the next, like v1)

| # | increment | verify (in the VM) |
|---|---|---|
| 0 | §4.0 swap-free notification (kernel, atrium-os branch) | **DONE, verified in-VM 2026-11-03**: after exit, `orig_bytes` matches `swapinfo` (8.1 MiB), and there was 0 corruption across 486k checksummed pages with blocks freed and reused mid-run. |
| 1 | §4.6 counters + `BIO_DELETE` + selftest on private slots | **DONE, verified in-VM 2026-11-03** (§3.1): `orig_bytes` matches `swapinfo` to 0.1 %, `trim(8)` frees exact slot counts, and the selftest is safe with swap live. |
| 2 | §4.2 packed allocator + §4.4 limit | **DONE, verified 2026-11-04**: the userspace churn test passes, a 550-page `dd` round-trip matches, the 1.9 GB fill has 0 corruption with pool overhead 3.9 % over the data, and the limit holds exactly (its feedback path waits for increment 5). |
| 3 | §4.3 per-CPU contexts + §4.1 hashed locks | **DONE, verified in-VM 2026-11-04**: 4 parallel `dd` writers 2.2× faster than increment 2, all streams byte-identical; the 1.9 GB fill has 0 corruption and 0 `fail_nomem` (after the span-header fix, §4.3). The 4-job-build PSI comparison is not run yet: a build under TCG takes hours, so it waits for a faster host. |
| 4 | §4.1 dynamic sizing, lazy table | **DONE, verified in-VM 2026-11-04**: module memory at load fell from 12.0 MiB to 99 KiB, a 4 GiB device works on a 2 GiB VM, and the 1.9 GB fill has 0 corruption and 0 failures. The slot is 16 bytes, not 8 (the 8-byte handle moves to increment 5). |
| 5 | §4.5 backing store and writeback | With a disk partition as backing: RAW pages land on disk, idle pages are written back after the period, and reads of written-back pages match. `swapinfo` shows only zram0. |
| 6 | §4.7 LZ4 option | Selftest under both codecs. In-kernel timing of codec vs whole `zram_strategy` per page (dtrace `fbt`). |

## 6. The VM-native decision (macOS shape), and what would trigger it

After increment 6, measure with dtrace where a page-out's time goes: `swap_pager`
and GEOM bookkeeping versus `zram_strategy` (codec plus allocator). Revisit a
VM-native compressor (a `swap_pager` hook in front of disk swap, Linux zswap
style, or a full compressor pager) **only if** the swap/GEOM share exceeds ~25 %
of the per-page cost under a pressure workload, or if writeback through zram's
own backing store proves unworkable. Otherwise the leaf-module design stays. It
needs no fork of `vm_pageout` or `swap_pager`, a bug in it can't corrupt the VM,
and it survives upstream merges.

## 7. Measuring in the VM (the real numbers)

Build memprobe natively in the VM (`cargo build --release`). The default `zstd`
feature compiles C, which the macOS cross-build cannot do; cross-builds use
`--no-default-features` (LZ4 only).

```sh
# Navigator: real corpus through real jailed workers, probe while they hold documents
memprobe watch --interval 1 --out /root/nav.csv --probe-every 20 --name navigator-work \
    -- jailed_corpus /root/recordings /root/worker-app /root/portcullis exec --instance {instance}

# A build under a RAM cap, with zram the only swap
memprobe watch --interval 1 --out /root/build.csv --probe-every 30 --name rustc --sample 4 \
    -- cargo build --release -j4

# One-off
memprobe snapshot --all --min-mb 20 --hist
```

On FreeBSD the probe attaches with `ptrace` and reads **only fully resident**
anonymous regions, because reading a non-resident page would fault a fresh zero
page into the target. The *coverage* line reports what it skipped. jemalloc
leaves most heap regions partly resident (only 0.4 of 42 MB was read for the
Navigator workers), so in practice use `--fault-partial`. It reads everything,
then subtracts the faulted-in pages, which are never-touched zeros, from the
zero count. That estimate is exact when nothing is swapped. It still grows the
target's RSS, so probe before a pressure run, not during one. The kernel
counters from increment 1 are the ground truth for what actually reaches the
store.

## 8. Deferred / non-goals

- **GPU VRAM as overflow**: rejected (see the header). Don't re-propose.
- **Dedup of identical pages**: ~15 % for the browser and ~0 elsewhere (§3).
  Reconsider with real Navigator and browser numbers from the VM.
- **zstd recompression of idle LZ4 pages**: only if increment 6 shows LZ4 is
  wanted as the front codec.
- **Hardware compression engines**: none on the target machines; out of scope.
- **Freeing empty slot-table chunks**: chunks stay until unload (§4.1 "As
  built"). Worth doing only if a large `disksize` with bursty use shows table
  memory that matters.
