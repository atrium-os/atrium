# atrium-zram v2: hardening the compressed-RAM tier

> **Status (2026-11-03):** increment 1 (counters, §4.6) is **built and
> verified in the FreeBSD VM**, together with BIO_DELETE support and a selftest
> fix. The first FreeBSD run (§3.1) found a v1 problem bigger than any in the
> original list: **pages freed by the VM stay in the store** (§1 item 7).
> **Increment 0 (§4.0) fixes it in the kernel** (atrium-os/freebsd-src
> `da0b30649`), verified in-VM the same day. The §3 numbers come from a Linux host
> used as a stand-in. §3.1 covers what has been confirmed on FreeBSD so far.
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
   succeed. A failed store fails the swap write.
3. **Fixed size.** It is 2 GiB of swap (`ZRAM_NSLOTS`), and the 12 MiB slot
   table is wired at load whether or not it is used.
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

### 4.3 Per-CPU codec contexts

One pre-allocated `ZSTD_CCtx` and `ZSTD_DCtx` per CPU (`mp_ncpus`), each behind
its own mutex. A bio uses `curcpu`'s context. If the thread migrates, it is still
correct because the context is locked, not pinned. There is no `malloc` on the
path, as in v1. The compression scratch buffer becomes per-context too (in v1 it
is a static guarded by the global mutex).

### 4.4 Memory limit

`kern.zram.mem_limit` (bytes; default 25 % of physical RAM; 0 = unlimited) caps
the physical bytes the store may hold. Over the limit, a write goes to writeback
(§4.5) or fails with `ENOSPC`. That makes swap full, which is what PSI `full` and
`memoryd` already react to. It keeps the compressed tier from consuming the
memory it is meant to save.

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
| 2 | §4.2 packed allocator + §4.4 limit | Selftest extended with 10k random-size objects, alloc/free, and a bitmap audit. `dd` round-trip. A build under a RAM cap: `pool_bytes / orig_bytes` within 5 % of memprobe's *packed* prediction. |
| 3 | §4.3 per-CPU contexts + §4.1 hashed locks | 4 parallel `dd` streams scale. Under a 4-job build with zram, lower PSI `full` than v1 at the same cap. |
| 4 | §4.1 dynamic sizing, lazy table | Wired memory at load drops by the table size. A disksize tunable over RAM works. |
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
