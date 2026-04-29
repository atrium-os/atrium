# Tessera Implementation Plan

> **Status:** non-normative implementation plan, version 1.
> **Companion to:** [tessera-fs.md](tessera-fs.md) (on-disk format) and [tessera-vfs.md](tessera-vfs.md) (POSIX mapping).
>
> This document specifies *how* Tessera is built. The format and behavior specs say *what* it does; this says *what code we write, in what order, with what tests*. Spec docs are normative; this plan is advisory and may be revised as work progresses.

## 0. Goals and non-goals

### Goals

- **Single canonical format implementation.** The bytes on disk are produced by exactly one codec, used by both kernel and userspace. No drift.
- **Production-quality crash safety.** Every sync point is tested; recovery is exhaustively crash-injected.
- **Ship an implementation that matches Atrium's stated language policy** (kernel C, userspace Rust, public APIs C ABI).
- **Bound the kernel-module surface.** The kmod is VFS adapter + buffer-cache glue; it does not contain algorithmic logic that could equally live in userspace.
- **Bound the bug-search space.** The most error-prone code (codecs) is in the smallest, most heavily tested module.

### Non-goals (v1)

- Multi-device volumes, RAID-style replication.
- Online resize.
- Quotas.
- Compression of blob bytes (format reserved; not implemented).
- At-rest encryption (format reserved; not implemented).
- Distributed/networked Tessera (single-host only).
- A bootable Tessera root partition (boots from UFS; Tessera mounts at `/`).

## 1. Architecture decision

Recapping the choice locked in by [docs/LANGUAGE-POLICY.md](../LANGUAGE-POLICY.md) and the prior design discussion:

```
   ┌─ tessera-core ────────────────────── C, freestanding ───────┐
   │   format codec, CDC, B+tree, manifest, pack, journal,        │
   │   GC algorithm, free-extent allocator, hash/CRC dispatch     │
   └────────┬─────────────────────────────────┬───────────────────┘
            │ direct linkage                  │ via cbindgen + FFI
            ▼                                 ▼
   ┌─ tessera-fs.ko ─────── C ────┐  ┌─ tessera-rs ────── Rust ───┐
   │   FreeBSD kmod:               │  │   safe wrapper crate;       │
   │   vop_*, mount, journal I/O,  │  │   used by all userspace     │
   │   buffer cache integration    │  │   tools.                    │
   └───────────────────────────────┘  └─────────────────────────────┘
                                              │
                                              ▼
                                       ┌─ userspace tools (Rust) ─┐
                                       │   mkfs / fsck / scrub /   │
                                       │   pin / subvol / repack / │
                                       │   stat / debug            │
                                       └───────────────────────────┘
```

**Single source of truth**: the C `tessera-core` library. Both the kmod and the Rust tools call into it; neither reimplements the codec.

**Rust contributes**: ergonomic CLI, async I/O, JSON output, and (critically) the property-test and crash-injection harnesses that hammer the C core through FFI. The strongest test infrastructure lives in Rust; the strongest production code lives in C; they meet at a stable C ABI.

## 2. Repository layout

A new top-level directory `atrium-tessera/` in [`atrium-os/atrium`](https://github.com/atrium-os/atrium).

```
atrium-tessera/
├── core/                           tessera-core (C library)
│   ├── include/tessera/
│   │   ├── tessera.h               public umbrella header
│   │   ├── format.h                on-disk struct layouts (tessera-fs.md §3-§10)
│   │   ├── codec.h                 encode/decode functions
│   │   ├── cdc.h                   FastCDC chunking
│   │   ├── btree.h                 B+tree primitives
│   │   ├── manifest.h              manifest build/parse
│   │   ├── pack.h                  pack-file codec
│   │   ├── journal.h               journal record codec + replay
│   │   ├── gc.h                    mark-sweep + repack
│   │   ├── extent.h                free-extent allocator
│   │   ├── hash.h                  SHA-256 / CRC32 dispatch
│   │   └── error.h                 error codes
│   ├── src/
│   │   ├── codec.c
│   │   ├── cdc.c
│   │   ├── btree.c
│   │   ├── manifest.c
│   │   ├── pack.c
│   │   ├── journal.c
│   │   ├── gc.c
│   │   ├── extent.c
│   │   ├── hash.c
│   │   └── crc.c
│   ├── tests/                      C unit tests (cmocka)
│   │   ├── test_codec.c
│   │   ├── test_cdc.c
│   │   ├── test_btree.c
│   │   ├── test_manifest.c
│   │   ├── test_pack.c
│   │   ├── test_journal.c
│   │   ├── test_gc.c
│   │   └── ...
│   ├── Makefile                    bsd.lib.mk based; produces libtessera-core.a
│   └── README.md
│
├── kmod/                           FreeBSD kernel module
│   ├── tessera_fs.c                vop_* dispatch table + mount/unmount
│   ├── tessera_vnode.c             vnode-specific ops (open, read, write, ...)
│   ├── tessera_dir.c               directory ops (lookup, readdir, mkdir, rmdir, rename)
│   ├── tessera_inode.c             inode-table interface from kmod (uses core/btree.h)
│   ├── tessera_io.c                buffer cache + journal I/O glue
│   ├── tessera_xattr.c             extended attribute ops
│   ├── tessera_subvol.c            subvolume ioctls
│   ├── tessera_pin.c               GC-root pinning ioctls
│   ├── Makefile                    bsd.kmod.mk; depends on core/ via SRCS
│   └── README.md
│
├── rs/                             Rust crates
│   ├── tessera-sys/                low-level FFI to tessera-core
│   │   ├── build.rs                links libtessera-core.a; bindgen for headers
│   │   ├── src/lib.rs              raw `unsafe extern "C"` bindings
│   │   └── Cargo.toml
│   ├── tessera/                    safe Rust API on top of tessera-sys
│   │   ├── src/
│   │   │   ├── lib.rs              re-exports
│   │   │   ├── codec.rs            safe wrappers around codec
│   │   │   ├── manifest.rs         safe wrappers around manifest
│   │   │   ├── pack.rs             safe wrappers around pack
│   │   │   ├── journal.rs          safe wrappers around journal
│   │   │   ├── volume.rs           Volume struct: open + ops
│   │   │   ├── error.rs            Rust error type
│   │   │   └── tests/              unit tests using safe API
│   │   └── Cargo.toml
│   └── tessera-test-harness/       userspace block-device simulator + crash injector
│       ├── src/
│       │   ├── lib.rs
│       │   ├── blockdev.rs         in-memory/file-backed block device
│       │   ├── inject.rs           crash injection at sync points
│       │   └── prop.rs             proptest generators
│       └── Cargo.toml
│
├── tools/                          Rust binaries
│   ├── mkfs-tessera/
│   ├── fsck-tessera/
│   ├── tessera-pin/
│   ├── tessera-scrub/
│   ├── tessera-subvol/
│   ├── tessera-repack/
│   ├── tessera-stat/
│   └── tessera-debug/
│
├── tests/                          integration + cross-cutting tests
│   ├── property/                   Rust proptest suites against tessera-core via FFI
│   ├── crash/                      crash-injection scenarios
│   ├── differential/               UFS-vs-Tessera POSIX equivalence
│   ├── kernel/                     ATF tests (run in mfsBSD VM)
│   └── benchmarks/                 perf microbenches
│
├── docs/
│   ├── implementation-notes.md     ongoing design decisions, deferred questions
│   └── benchmarks.md               recorded perf numbers per release
│
└── README.md                       top-level overview
```

## 3. Build system

Three build paths, each independently runnable:

### 3.1 `tessera-core` userspace build

`atrium-tessera/core/Makefile`:

```make
LIB=        tessera_core
SHLIB_MAJOR=1
SRCS=       codec.c cdc.c btree.c manifest.c pack.c journal.c \
            gc.c extent.c hash.c crc.c
INCS=       include/tessera/*.h

CFLAGS+=    -O2 -Wall -Wextra -Wpedantic -Werror \
            -fno-strict-aliasing \
            -I${.CURDIR}/include

# SHA-256 / CRC32 hardware acceleration via libmd (FreeBSD base)
LIBADD=     md

.include <bsd.lib.mk>
```

Produces `libtessera_core.a` (static) and `libtessera_core.so.1` (shared, for tools that prefer dynamic linkage). Userspace tools statically-link by default.

### 3.2 `tessera-fs.ko` kernel-module build

`atrium-tessera/kmod/Makefile`:

```make
KMOD=       tessera_fs
SRCS=       tessera_fs.c tessera_vnode.c tessera_dir.c tessera_inode.c \
            tessera_io.c tessera_xattr.c tessera_subvol.c tessera_pin.c
SRCS+=      device_if.h bus_if.h vnode_if.h

# Link in tessera-core sources directly (kmod can't link against userspace .a)
SRCS+=      ${.CURDIR}/../core/src/codec.c \
            ${.CURDIR}/../core/src/cdc.c \
            ${.CURDIR}/../core/src/btree.c \
            ${.CURDIR}/../core/src/manifest.c \
            ${.CURDIR}/../core/src/pack.c \
            ${.CURDIR}/../core/src/journal.c \
            ${.CURDIR}/../core/src/gc.c \
            ${.CURDIR}/../core/src/extent.c \
            ${.CURDIR}/../core/src/hash.c \
            ${.CURDIR}/../core/src/crc.c

CFLAGS+=    -I${.CURDIR}/../core/include \
            -DTESSERA_KERNEL=1

CWARNFLAGS+= -Wno-unused-parameter

.include <bsd.kmod.mk>
```

The `TESSERA_KERNEL` define switches `tessera-core` to use `kmem_*` allocators, `mtx_*` locks, and FreeBSD's in-kernel SHA-256 (`<sys/sha256.h>`) instead of userspace equivalents. Conditionally-compiled wrappers in `core/include/tessera/*.h` and `core/src/*.c` handle the dual-target builds.

### 3.3 Rust crates

`atrium-tessera/rs/tessera-sys/build.rs` is the FFI build script:

```rust
fn main() {
    // Build libtessera_core.a if not present
    // (or rely on a parent Makefile to have built it)
    println!("cargo:rustc-link-search=native=../../core/obj");
    println!("cargo:rustc-link-lib=static=tessera_core");
    println!("cargo:rustc-link-lib=md");

    // Generate Rust bindings from C headers
    let bindings = bindgen::Builder::default()
        .header("../../core/include/tessera/tessera.h")
        .clang_arg("-I../../core/include")
        .generate()
        .expect("bindgen failed");
    bindings.write_to_file(
        std::path::Path::new(&std::env::var("OUT_DIR").unwrap())
            .join("bindings.rs"),
    ).unwrap();
}
```

`tessera-sys` exposes raw `extern "C"` bindings; `tessera` wraps them in a safe API; tools depend only on `tessera`.

Cross-compile to `aarch64-unknown-freebsd` works the same way it does for the existing Atrium Rust crates (via the project-level `.cargo/config.toml`).

### 3.4 Top-level orchestration

A simple `Makefile.tessera` at the atrium-tessera root drives `make all`:

```make
SUBDIR=     core kmod
RUSTFLAGS=  --release --target=aarch64-unknown-freebsd

build:
    cd core && make
    cd kmod && make
    cd rs && cargo build $(RUSTFLAGS)
    cd tools && cargo build $(RUSTFLAGS)

test:
    cd core && make check
    cd rs && cargo test
    cd tests && cargo test --release
```

CI (GitHub Actions on the atrium-os/atrium repo) runs `make test` on every commit; `make build` for the kmod is verified by cross-compile against the FreeBSD sysroot.

## 4. Component boundaries and APIs

### 4.1 tessera-core public API

A handful of opaque-pointer types and explicit functions. Sample slices:

```c
// Manifest construction
typedef struct tessera_manifest_builder tessera_manifest_builder_t;

tessera_manifest_builder_t *tessera_manifest_begin(uint8_t kind);
int tessera_manifest_add_chunk(tessera_manifest_builder_t *,
                                const uint8_t hash[32],
                                uint64_t logical_offset,
                                uint32_t size);
int tessera_manifest_finalize(tessera_manifest_builder_t *,
                               uint8_t *out_buffer,
                               size_t buffer_len,
                               size_t *out_size,
                               uint8_t out_hash[32]);
void tessera_manifest_free(tessera_manifest_builder_t *);

// CDC
typedef struct tessera_cdc tessera_cdc_t;
tessera_cdc_t *tessera_cdc_new(uint32_t avg_chunk, uint32_t min_chunk, uint32_t max_chunk);
int tessera_cdc_chunk(tessera_cdc_t *, const uint8_t *data, size_t len,
                       size_t *out_boundaries, size_t *out_count);
void tessera_cdc_free(tessera_cdc_t *);

// Hash dispatch (with hardware acceleration)
void tessera_sha256(const uint8_t *data, size_t len, uint8_t out[32]);
uint32_t tessera_crc32(const uint8_t *data, size_t len);

// B+tree primitives
typedef struct tessera_btree tessera_btree_t;
tessera_btree_t *tessera_btree_open(uint32_t key_size, uint32_t value_size,
                                      tessera_block_io_t *io);
int tessera_btree_get(tessera_btree_t *, const void *key, void *value_out);
int tessera_btree_put(tessera_btree_t *, const void *key, const void *value);
int tessera_btree_delete(tessera_btree_t *, const void *key);
void tessera_btree_close(tessera_btree_t *);

// Pack codec
int tessera_pack_write(uint8_t *buf, size_t buf_len,
                        const tessera_pack_descriptor_t *desc,
                        const uint8_t **blob_data, const size_t *blob_sizes,
                        const uint8_t (*blob_hashes)[32], size_t n_blobs);
int tessera_pack_read_index(const uint8_t *buf, size_t buf_len,
                             tessera_pack_index_entry_t *out_entries,
                             size_t *n_entries);

// Journal
typedef struct tessera_journal tessera_journal_t;
tessera_journal_t *tessera_journal_open(tessera_block_io_t *io,
                                          uint64_t start_sector, uint64_t length_sectors);
int tessera_journal_replay(tessera_journal_t *, tessera_replay_callback_t, void *ctx);
int tessera_journal_append_record(tessera_journal_t *, const tessera_record_t *);
int tessera_journal_commit_tx(tessera_journal_t *, uint64_t tx_id);
void tessera_journal_close(tessera_journal_t *);

// And ... lots more for inodes, dirs, GC, extent allocator
```

Conventions:
- All functions return `0` on success, negative `tessera_errno_t` on failure.
- All allocation is explicit: caller provides buffers wherever possible. The library doesn't malloc internally except for opaque-state structs (which have explicit `*_free`).
- No global state. Every function takes a context pointer.
- Thread safety: all mutating calls require the caller to hold the appropriate lock; reads are safe to call concurrently from multiple threads.

### 4.2 tessera-core ↔ kmod boundary

The kmod uses tessera-core directly; no FFI. The boundary is *function calls within the same C compilation unit set*, glued via the `TESSERA_KERNEL` flag:

```c
// In core/src/hash.c
#ifdef TESSERA_KERNEL
#  include <sys/sha256.h>
   void tessera_sha256(...) { SHA256_Init(&ctx); ... }
#else
#  include <sha256.h>          // libmd userspace
   void tessera_sha256(...) { SHA256_Init(&ctx); ... }
#endif
```

Same source compiles to both targets. Allocator, lock primitives, log primitives all switch on `TESSERA_KERNEL`.

### 4.3 tessera-core ↔ Rust boundary

`tessera-sys` is the unsafe binding layer. `tessera` is the safe wrapper. Tools depend only on the safe layer.

Pattern (using `tessera-manifest` as an example):

```rust
// In rs/tessera/src/manifest.rs
use tessera_sys::*;

pub struct ManifestBuilder {
    raw: *mut tessera_manifest_builder_t,
}

impl ManifestBuilder {
    pub fn new(kind: ManifestKind) -> Self {
        let raw = unsafe { tessera_manifest_begin(kind as u8) };
        assert!(!raw.is_null());
        Self { raw }
    }

    pub fn add_chunk(&mut self, hash: [u8; 32], offset: u64, size: u32)
        -> Result<(), TesseraError>
    {
        let r = unsafe {
            tessera_manifest_add_chunk(self.raw, hash.as_ptr(), offset, size)
        };
        if r != 0 { Err(r.into()) } else { Ok(()) }
    }

    pub fn finalize(self, buf: &mut [u8]) -> Result<([u8; 32], usize), TesseraError> {
        let mut out_size = 0;
        let mut out_hash = [0u8; 32];
        let r = unsafe {
            tessera_manifest_finalize(self.raw, buf.as_mut_ptr(),
                                       buf.len(), &mut out_size, out_hash.as_mut_ptr())
        };
        if r != 0 { Err(r.into()) } else { Ok((out_hash, out_size)) }
    }
}

impl Drop for ManifestBuilder {
    fn drop(&mut self) {
        unsafe { tessera_manifest_free(self.raw); }
    }
}
```

Errors flow through a single `TesseraError` enum mapping `tessera_errno_t` values to Rust variants.

## 5. Implementation phases

Phases are ordered by dependency; some run in parallel.

### Phase 0 — Project setup (1 week)

- Create `atrium-tessera/` skeleton in the atrium repo.
- Set up `core/Makefile`, `kmod/Makefile`, `rs/Cargo.toml`, top-level `Makefile.tessera`.
- Wire CI: build `core/`, build `kmod/` cross-compiled, build `rs/` cross-compiled, run `core/tests/`, run `rs/cargo test`. All green.
- Stub headers in `core/include/tessera/*.h` so subsequent phases can fill them in.
- First test: `make check` runs zero tests successfully.

### Phase 1 — `tessera-core` algorithm primitives (4–6 weeks)

Independent algorithmic pieces. Each is small and self-contained.

| Module | Estimate | Dependencies | Test focus |
|---|---|---|---|
| `hash.c` | 3 days | none | NIST test vectors; HW vs software path equivalence |
| `crc.c` | 2 days | none | RFC test vectors |
| `cdc.c` | 1 week | hash.c | byte-shift stability property; chunk-size-distribution sanity |
| `codec.c` (struct encode/decode) | 1.5 weeks | none | roundtrip of every struct in tessera-fs.md §3-§10 |
| `manifest.c` | 1 week | codec.c, cdc.c | round-trip of all manifest kinds; deterministic hash |
| `pack.c` | 1 week | codec.c, hash.c, crc.c | round-trip; bloom FPR ≤ 0.1%; ordered index |
| `extent.c` (free-extent allocator) | 1 week | none | randomized alloc/free; no overlap; coalescing |
| `btree.c` | 2 weeks | codec.c | random ops; balance preservation; key ordering |

Each module has its own C unit test file under `core/tests/`. A test passes when:
- All NIST/RFC test vectors pass.
- Code coverage of the module is ≥ 90% per `gcov`.
- Compiles with `-Werror` under `-Wall -Wextra -Wpedantic`.

Phase 1 deliverable: `make check` runs ~200 unit tests, all green; `libtessera_core.a` builds for both userspace and the kmod stub.

### Phase 2 — `tessera-rs` FFI bindings + first tools (3–4 weeks; parallel with later Phase 1)

Depends on Phase 1 modules existing (even with stubbed implementations).

- `tessera-sys`: FFI bindings via bindgen, regenerated on header changes.
- `tessera`: safe wrapper crate. One small test per binding to catch regressions.
- `tessera-test-harness`: in-memory block device simulator. Lets us exercise tessera-core without ever touching a real disk or kmod.
- `mkfs-tessera`: simplest tool. Takes a file (or block device), writes the format-time structures (superblocks, empty inode table with inodes 1+2, free-extent map, initial pack with empty manifests), unmounts cleanly.
- `tessera-debug`: the dumper tool. Reads any Tessera volume, prints structures human-readable. Critical for debugging Phase 3+.

Phase 2 deliverable: `mkfs-tessera /tmp/test.img && tessera-debug /tmp/test.img` produces sensible output; round-trip of every supported VFS operation through the harness works correctly.

### Phase 3 — Property + crash-injection harness (4–5 weeks; parallel with Phase 4)

This is the heaviest test infrastructure investment. Pays back tenfold during kmod development.

- `proptest` strategies: random sequences of `mkdir/create/write/read/delete/snapshot/restore/gc` ops. Generators produce valid op sequences with realistic distributions (many small files, occasional large files, mixed dedup ratios).
- Invariant suite: after any random op sequence, assert:
  - All written content is readable.
  - No reachable blob is missing from any pack.
  - GC output is idempotent (`gc(gc(x)) == gc(x)`).
  - Hashes are deterministic across processes.
  - POSIX deviations match the spec table (tessera-vfs.md §11).
- Crash injection: at every "should be durable" sync point, drop the world via the simulator; remount; verify converged consistent state.
- Differential: run identical ops against UFS (via `mksfs.ufs` + loop mount in test VM); assert POSIX-observable equivalence.

Phase 3 deliverable: a shaking-out test suite catches our own bugs before Phase 4 stresses them.

### Phase 4 — Kernel module (6–10 weeks)

Depends on Phase 1 (core primitives) and benefits from Phase 3 (test harness for shared algorithm bugs).

| Component | Estimate | Test |
|---|---|---|
| Mount/unmount lifecycle, journal replay | 1.5 weeks | mount/unmount cycles via ATF |
| `vop_lookup`, `vop_getattr`, `vop_open`, `vop_close` | 1 week | basic stat/open of static volume |
| `vop_read` (manifest tree walk + chunk fetch) | 1 week | read of inline + chunked + tree manifests |
| `vop_readdir` (synthesized "."/"..") | 1 week | readdir/readdir_r conformance |
| `vop_write` (per-fd buffer, flush at fsync/close) | 2 weeks | crash-injection with sync writes |
| `vop_create`, `vop_mkdir`, `vop_rmdir`, `vop_remove` | 1.5 weeks | full directory mutation tests |
| `vop_rename` (single-tx atomicity) | 1.5 weeks | atomic-rename torture |
| `vop_link`, `vop_symlink`, `vop_readlink` | 1 week | hardlink + symlink semantics |
| `vop_setattr` (chmod/chown/chflags/utimes/truncate) | 1 week | metadata-only mutation tests |
| `vop_getxattr / setxattr / listxattr / removexattr` | 1 week | xattr roundtrip + Atrium tag prefix |
| `vop_copy_file_range` (reflink) | 1 week | hash-only-copy; whole-file and partial |
| `vop_getpages / vop_putpages` (mmap) | 2 weeks | MAP_PRIVATE + MAP_SHARED workloads |
| Subvolume + GC-root ioctls | 1 week | tessera subvol + tessera-pin from CLI |

Each VFS-op lands incrementally. The `tessera-fs.ko` evolves from "mounts but everything returns ENOTSUP" to fully POSIX-conformant.

Phase 4 deliverable: standard `xfstests`-style FS conformance suite passes; mfsBSD VM rig boots, mounts a Tessera volume, runs Atrium apps from it.

### Phase 5 — Production tooling (3–4 weeks, parallel with Phase 4)

- `fsck-tessera`: full consistency checker (+ repair mode for derivable structures).
- `tessera-scrub`: blob-integrity walker; daemon variant `tessera-scrubd`.
- `tessera-repack`: GC + repack with `--gc / --full / --aggressive` modes.
- `tessera subvol create / promote / snapshot / rollback / diff / send / receive`.
- `tessera-pin add / rm / list`.
- `tessera-stat`: JSON volume stats.

These run against tessera-core via FFI; they don't need the kmod (most work on unmounted volumes). `tessera subvol` and `tessera-pin` integrate with the kmod via ioctls when the volume is mounted.

### Phase 6 — Performance + hardening (3–4 weeks; ongoing)

- Hardware acceleration verification (SHA-256, CRC32, AES-GCM stubs).
- Hot-path optimization: pack lookup, chunk cache, read-ahead in `vop_read`.
- Benchmark suite: cold-cache vs hot-cache reads, small-file write throughput, large-file sequential write, metadata ops (stat/readdir/unlink), dedup-ratio over realistic corpora.
- Memory pressure: 10M blob test; index lazy-loading.
- Long-running stress: 24-hour parallel-reader-writer-GC under `proptest` + crash injection.

Phase 6 produces the first `docs/benchmarks.md` with recorded numbers, which become the regression baseline.

### Phase 7 — Documentation, integration, release (1–2 weeks)

- Update [docs/ROADMAP.md](../ROADMAP.md): D1.5 done, D2 unblocked.
- Update [docs/subsystems/storage.md](../subsystems/storage.md) to reflect actual implementation (not the symlinks Phase 1 it currently describes).
- Atrium integration: switch `/home`, `/var/lib/atrium/jails/*` to Tessera-backed mounts.
- Opifex integration spec for diff-stream-driven package updates.
- Tag a v1.0 release.

## 6. Test strategy

Five tiers, covering different regression risks. All run in CI; long-running tiers gated on a separate cadence.

### 6.1 Unit tests (every commit)

- C unit tests in `core/tests/`. cmocka framework. ~200-300 tests after Phase 1.
- Rust unit tests in `rs/tessera/tests/`. ~50-100 tests covering safe wrappers and error paths.
- Run on every commit; ~1 minute total.

### 6.2 Property tests (every commit)

- Rust `proptest` suites in `tests/property/`. Random op sequences hammering tessera-core via FFI.
- Invariants checked after every sequence: no missing blobs, GC idempotent, hashes deterministic, content readback identical to writes.
- Default budget: 1000 cases per test, ~5 minutes total per CI run. Failure cases are minimized and saved.

### 6.3 Crash-injection (every commit, sampled; nightly, exhaustive)

- Rust harness in `tests/crash/`. Block-device simulator with sync-point hooks. At every sync, with some probability, drop subsequent writes.
- Per-commit: 100 random crash points sampled.
- Nightly: exhaustive enumeration of crash points across a diverse op-set; ~hours of runtime.

### 6.4 Differential (per-commit smoke; nightly full)

- Rust harness in `tests/differential/`. Same op sequence against UFS and Tessera; assert POSIX-observable equivalence (modulo deviations spec).
- Per-commit: 50 ops × 20 test cases = 1000 ops sampled.
- Nightly: 5000 op sequences, longer per-case.

### 6.5 In-kernel ATF (nightly)

- ATF tests in `tests/kernel/`. Run inside an mfsBSD VM with the kmod loaded.
- Coverage: `xfstests`-shaped POSIX conformance, kmod-specific regression tests for every fixed bug.
- Slowest tier; ~30 minutes per run.

### 6.6 Long-running stress (weekly)

- 24-hour mixed-workload runs with random crash injection. Multiple VMs, different seeds.
- Reports filed against `docs/implementation-notes.md`.

## 7. Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| POSIX `mmap(MAP_SHARED)` semantics under COW | high | high | early prototype + ATF tests; documented deviation if needed |
| Deadlock in kmod lock hierarchy | high | high | strict lock ordering documented in `kmod/LOCKING.md`; lockdep-style checker in debug builds |
| Crash-recovery bugs missed by tests | medium | catastrophic | crash-injection coverage of every sync point; user-visible "was last unmount clean?" indicator |
| Memory pressure at 10M+ blobs | medium | high | benchmark gate at Phase 6; lazy index loading; eviction strategy |
| Performance regression vs UFS | medium | medium | benchmark suite gates merges; hardware crypto verified |
| Rust-FFI testing infrastructure complexity | low | medium | kept simple by limiting `unsafe` to `tessera-sys` only |
| Format spec ambiguity discovered late | medium | medium | spec is locked; ambiguities trigger spec amendment with version bump |
| FreeBSD VFS API churn | low | medium | targeted at FreeBSD 16.0-CURRENT; track API changes per release |
| Cross-compile + kmod-build environment | medium | low | already proven for atrium-virtio-gpu / atrium-bootfb |
| In-kernel SHA-256 API differences across FreeBSD versions | low | low | wrapper macro in `core/src/hash.c` |

## 8. Tooling

Beyond the userspace tools shipped with Tessera itself:

- **bindgen** for Rust FFI generation.
- **cbindgen** for any reverse direction (Rust constants → C headers; only needed if tools want to share constants).
- **cmocka** or **minunit** for C unit testing.
- **proptest** for Rust property testing.
- **gcov** + **lcov** for coverage reports (per-module gate of ≥ 90%).
- **clang-tidy** for static analysis on the C core.
- **MIRI** for Rust unsafety checks on `tessera-sys`.
- **valgrind** in userspace for tool memory checks.
- **CI**: GitHub Actions on the atrium-os/atrium repo. Matrix: amd64 + aarch64 build; userspace tests; cross-compile kmod (build only); ATF in nightly mfsBSD VM.

## 9. Time estimate

| Phase | Duration | Critical path |
|---|---|---|
| 0. Setup | 1 wk | y |
| 1. tessera-core | 4–6 wk | y |
| 2. Rust + first tools | 3–4 wk | parallel after week 2 of Ph 1 |
| 3. Property/crash harness | 4–5 wk | parallel; depends on Ph 1 + 2 |
| 4. Kernel module | 6–10 wk | y |
| 5. Production tooling | 3–4 wk | parallel with Ph 4 |
| 6. Perf + hardening | 3–4 wk | y |
| 7. Docs + release | 1–2 wk | y |

**Critical path total**: 18–28 weeks (4.5–7 months).
**Wall-clock with parallelism**: ~5–6 months for a single committed engineer; potentially 4 months with 1.5 engineers (Phase 2/3 split with the test infrastructure work).

This matches the earlier 6-7 month estimate I gave, with phases now scoped explicitly.

## 10. Out-of-band integration work (not in Tessera itself)

These touch other Atrium components and should be tracked separately, not blocked on Tessera v1.0:

- **Opifex** package manager changes to use Tessera diff streams for incremental updates.
- **Portcullis** jail launcher to mount Tessera subvolumes per jail.
- **Boot configuration** to mount root from Tessera (after a kernel boot from a small UFS `/boot`).
- **Migration tools**: a one-off `ufs-to-tessera` converter that takes an existing FreeBSD install and produces a Tessera-backed copy.
- **Atrium installer** updates to format new disks as Tessera by default.

Each is its own project; D1.5 scope is "Tessera works as a mountable, POSIX-correct, performance-acceptable filesystem." The above are D2/D3+ work that builds on it.

## 11. Decision points open during implementation

These are tracked in `docs/implementation-notes.md` as they come up; this section enumerates known ones at plan-time:

- **Default CDC parameters**: 64 KiB avg / 16 KiB min / 256 KiB max are the spec defaults, but real workload measurement may push these. Re-tune in Phase 6 based on benchmark data.
- **Pack soft-cap (64 MiB)**: arbitrary; revisit if many-small-pack overhead dominates.
- **GC repack threshold (50%)**: live-blob ratio at which a pack is repacked. Tunable; revisit.
- **Inode-table B+tree fan-out**: 29 entries per leaf is fixed by format, but internal-node fan-out depends on key+pointer sizes; revisit if tree depth dominates lookup cost.
- **Page cache strategy**: simple LRU vs ARC. Phase 4 starts with LRU; Phase 6 may switch.
- **Compression in v2**: zstd at what level, by default? Selected per pack-kind?

These are deferred from the spec; they're operational tunables, not format-affecting choices.

---

End of v1 implementation plan. The plan tracks against this document; substantial deviations (component reorganization, phase reordering) get plan-version bumps; minor tactical changes are noted in `docs/implementation-notes.md`.
