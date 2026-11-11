# Rust kernel-module spike

Step 3 of [docs/reviews/2026-11-07-freebsd-kernel-security-rust.md](../docs/reviews/2026-11-07-freebsd-kernel-security-rust.md).
The question it answers is D2: should new Atrium kernel code be Rust by default?

**Pass criteria (Shankar, 2026-11-07):** a minimal Rust kmod on arm64 and amd64, using safe
wrappers for malloc, locks, copyin/copyout and a device node. It must be KASAN clean, survive
1 h of syzkaller, and show no measurable latency cost against the same module in C.

## 1. What is here

| path | what |
|------|------|
| `spike_abi.h` | the shared ABI: `/dev/{c,r}spike`, five ioctls (NOP, SET/GET a blob, PARSE a nested record stream, STATS), and a 4 KiB FIFO through read/write |
| `cspike/cspike.c` | the C module, written the way Atrium's C kmods are (cdevsw, sx + mtx, `copyin` into `malloc(9)` memory) |
| `rspike/rspike.rs` | the Rust module, the same behaviour, `#![deny(unsafe_code)]`: the only `unsafe` is two marker-trait impls whose layout is checked by `const` asserts |
| `bench/spike-bench.c` | `test`: differential test, the same seeded random and malformed inputs to both devices, every rc, errno and output must agree. `bench`: latency per operation, both devices interleaved |
| `syz/` | syzkaller descriptions for both devices (copied into syzkaller's `sys/freebsd`) |
| `kpi-patches/` | our patches to freebsd-kpi-rs (§3) |
| `../scripts/rust-kmod-spike.sh` | `prepare`, `build`, `deploy`, `test`, `bench`; cross-builds on the Linux host |

The record decoder is the part that matters: it is the kind of code (lengths from user space,
nesting, a container type) where C kmods get advisories. In Rust it indexes slices, so a bad
length is either rejected or a bounds-check panic, never a read of adjacent kernel memory.

## 2. Bindings and licences

Checked against [docs/LICENSING-POLICY.md](../docs/LICENSING-POLICY.md) (vendored or forked
dependencies: MIT, BSD-2/3, Apache-2.0, ISC, Zlib allowed).

| project | licence | state | verdict |
|---------|---------|-------|---------|
| **ayrtonm/freebsd-kpi-rs** (Ayrton Muñoz) | every source file has an `SPDX-License-Identifier: BSD-2-Clause` header; **the repository has no LICENSE file**, and the build glue (`kern.rust.mk`, `kmod.rust.mk`, `kern.rustroot.mk`), `kpi/src/net.rs` and `patches/*.diff` carry no notice | active (545 commits, head 4434e53 on 2026-11-02), builds in-tree as `sys/rust`, targets arm64 and amd64 | **usable**: BSD-2 is allowed. Before we ship anything built on it, ask the author to add a LICENSE file and headers to the unlabelled files: requested by Shankar in an issue on the project, 2026-11-10 |
| asomers/freebsd-kernel-module-rust | BSD-2-Clause | old, out-of-tree, cargo-based | not used |
| kev009/rustkpi | BSD-2-Clause (fork of Linux work) | dormant | not used |
| freebsd-kmi-rs | custom licence | | not allowed |

The Rust toolchain parts that end up in the module (libcore, compiler-builtins) are
MIT/Apache-2.0. bindgen is a build tool (BSD-3), not shipped.

## 3. Our patches to freebsd-kpi-rs

Applied by `rust-kmod-spike.sh prepare` on top of `KPI_REV`. Each is written to be offered
upstream.

1. **`usercopy.rs`**: `UserSlice` (a user address and length that can only be passed to
   `copyin`/`copyout`), `FromBytes`/`AsBytes` marker traits for typed copies, and `IoctlData`,
   the ioctl argument buffer with size-checked typed access. Under KASAN the copies go through
   `kasan_copyin`/`kasan_copyout`.
2. **`d_ioctl` in `CDevSw`**, and a soundness fix found while writing it: upstream's
   `uiomove_read` took `&mut [u8]` and `uiomove_write` took `&[u8]`, the wrong way round, so a
   write to the device wrote through a shared reference (undefined behaviour). Lengths above
   `INT_MAX` are now refused.
3. **`Drop` for mutexes and sx locks**: upstream never called `mtx_destroy`/`sx_destroy`, so
   unloading a module left the locks registered with WITNESS.
4. **`kern.rust.mk`**: libcore from rustup's `rust-src` (no separate Rust source checkout),
   KASAN instrumentation (`-Zsanitizer=kernel-address` with FreeBSD's shadow offset) when the
   kernel has `options KASAN`, and `-Zbranch-protection=bti,pac-ret` on arm64 (upstream had
   BTI only, so Rust code lost the PAC return-address protection the C kernel has).
5. **Cheaper `Vec`**: allocation, fill and `copy_in_vec` without a zero fill first (see §5).
6. **File objects, kqueue, vmem, sleep, cdevpriv** (for the atrium-gpu-amd front-end,
   `atrium-gpu-amd/rs/README.md`): `file.rs` puts a `Box<T>` behind a `struct file` (`install`,
   a type-checked `FileRef<T>` from an fd, default fileops whose `fo_close` frees the box);
   `event.rs` is `selinfo`/`knlist` on an existing mutex plus knote helpers; `vmem.rs` wraps
   vmem arenas; `mtx.rs` gains `msleep_sbt` (returning the raw errno, since `ErrCode` cannot hold
   `ERESTART`) and `sbinuptime`; `cdev.rs` gains `devfs_set_cdevpriv`/`devfs_get_cdevpriv` for a
   boxed per-open state.
7. **Per-module bindgen** in `kmod.rust.mk`: a module names a C header (`RUST_BINDGEN_HDR`) and an
   allowlist (`RUST_BINDGEN_ALLOW`), and the crate `include!`s the result on top of
   `kpi::bindings`. Also blocklists `ktr_get_tracevp`, whose inline wrapper broke the kpi
   bindings once `sys/file.h` was included.
8. **amd64 KASAN shadow offset**: 0004 set the offset for arm64 only, so on amd64 rustc used its
   user-space default and the first stack poison faulted (found 2026-11-11, the first amd64
   KASAN run of a Rust module). Now `0xdffff7c000000000`, the offset clang uses for the C kernel.
9. **Managed device pagers** (`pager.rs`): `OBJT_MGTDEVICE` objects for `d_mmap_single` that the
   driver can revoke. The handle (`Box<T>`) belongs to the object and is freed by the pager
   destructor. `T` only picks the physical address for a fault, and the fault path refuses any
   page that is not an unowned fictitious page. atrium-gpu-amd's doorbell grants use it.
10. **sx additions**: `sx_init_flags` (`SX_DUPOK`), `sx_try_xlock`, and `leak`/`xunlock_leaked`
    for a hold that spans a prepare/restore pair of calls from C (atrium-gpu-amd's reset).

## 4. Results (arm64 2026-11-07, amd64 2026-11-10; QEMU TCG on the cloud host)

| check | arm64 | amd64 |
|-------|-------|-------|
| builds against ATRIUM-NODEBUG / -KASAN / -SYZKALLER, loads, unloads, reloads | pass | ATRIUM-NODEBUG: pass (needs LLVM 19, see below) |
| differential test, ATRIUM-NODEBUG | 1,000,000 iterations + FIFO, 0 mismatches | 1,000,000 iterations + FIFO, 0 mismatches (after the GET fix below) |
| differential test, ATRIUM-KASAN (Rust code instrumented: 48 `__asan` call sites) | 200,000 iterations, 0 mismatches, no KASAN report | not run here; the atrium-gpu-amd run (2026-11-11) found that amd64 KASAN needed patch 0008 |
| syzkaller, ATRIUM-SYZKALLER (KASAN + KCOV), both devices | 1 h, 8,204 programs, 72-program corpus, 2,058 coverage points, **0 crashes**. Slow: TCG plus KASAN plus KCOV runs about 2 programs a second. A run on real timing would cover far more | not run |
| latency, Rust/C (median of 11 × 200,000 calls, 3 runs, HVF on the Mac) | nop 1.02–1.03×, set64 0.99–1.03×, get64 1.03–1.04×, write+read64 1.03×, parse256 0.85× (see §5) | not measured |

What the spike turned up on the way, all fixed and written to be offered upstream:

- **FreeBSD:** zero-initialised stack variables (`INIT_ALL=zero`) hang arm64 at boot, the
  same ifunc problem FreeBSD already marks broken on amd64; and the arm64 KASAN runtime lacks
  the 64-bit bus_space write interceptors its own header declares, so Rust modules could not
  load on a KASAN kernel. Both in atrium-os/freebsd-src.
- **freebsd-kpi-rs:** the uiomove mutability swap (undefined behaviour on every device write),
  locks never destroyed, BTI without PAC, no KASAN support (patches 0002–0004).
- **amd64 toolchain:** clang 18 emits `R_X86_64_GOTPCRELX` relocations that `kldload` refuses,
  so amd64 modules are built with LLVM 19 (`rust-kmod-spike.sh` picks it; `LLVM=` overrides).
- **A twin divergence only amd64 showed:** `SPIKE_IOC_GET` with nothing to copy and a
  kernel-range user pointer returned 0 from C and `EFAULT` from Rust (3 in 1,000,000). C skips
  `copyout` when the length is 0; the Rust version called it, and amd64's `copyout` checks the
  address even for 0 bytes where arm64's returns at once. The Rust module now skips it too. Not
  a memory-safety issue, but it is the kind of difference the differential test exists to find.
- **syzkaller:** its isolated-VM mode streams the console with Linux `dmesg -w`; on FreeBSD the
  guest needs a small wrapper (`/root/bin/dmesg`, RUNBOOK §13).

The Rust module needed no `unsafe` beyond the two layout promises, and compiled against the
patched bindings at the first attempt once its imports were right. Size: `rspike.ko` is
142 KB against `cspike.ko`'s 13 KB on arm64 and 124 KB against 16 KB on amd64 (NODEBUG), almost all of it the KPI crate's generic code
and bindgen wrappers that every Rust module carries once.

## 5. Latency (2026-11-10, Mac16,6, qemu `-accel hvf -cpu host -smp 4`, ATRIUM-NODEBUG)

`spike-bench bench 200000 11`, run three times, both devices interleaved and the order alternated
per round. Ratios are within one run. Absolute times differ between the two sessions by about
10% because the Mac's load differed (1.0 and 2.5), so compare them only within a run.

| op | C ns | Rust ns, first run | Rust/C | C ns | Rust ns, with patch 0005 | Rust/C |
|----|------|------|--------|------|------|--------|
| nop | 104 | 106 | 1.02× | 118 | 120 | 1.02–1.03× |
| set64 | 132 | 167 | **1.18–1.28×** | 148 | 152 | 0.99–1.03× |
| get64 | 131 | 135 | 1.04× | 146 | 152 | 1.03–1.04× |
| parse256 | 219 | 279 | **1.25–1.28×** | 241 | 204 | 0.85× |
| write+read64 | 228 | 234 | 1.03× | 256 | 264 | 1.03× |

The first run found a real cost, in the bindings rather than in the language. `copy_in_vec`
zeroed its buffer a byte at a time: `fill_to_capacity` called `push()`, which re-checks the
capacity for each element, so LLVM could not turn it into a `memset`. It also allocated through
`malloc_aligned(9)`, whose `malloc_domainset()` path sets up a domain iterator on every call.
Patch 0005 fixes both (raw fill loop; no zero fill before `copyin`; plain `malloc(9)` when the
alignment is 16 or less).

What remains:

- **About 2–4 ns per call (2–4%) on every operation.** This is fixed overhead in the generated
  glue: the argument struct is read byte by byte (`IoctlData::read` copies through an
  unaligned byte view), the ioctl number is decoded twice, and stats go through a closure. It
  is small next to the system-call cost and can be cut in the bindings.
- **parse256 at 0.85× is not a like-for-like win.** C still allocates with `M_ZERO` and Rust
  no longer zero-fills, so C pays to zero 256 bytes that `copyin` then overwrites. Without
  `M_ZERO` in C the two should be about level. The same applies to set64 with 64 bytes.

Verdict against the pass criterion "no measurable latency cost": not met in the strict sense,
since a fixed cost of 2–4% is measurable. The cost does not grow with the work done, and the
parser itself (bounds-checked slices) is no slower than the C pointer arithmetic.
