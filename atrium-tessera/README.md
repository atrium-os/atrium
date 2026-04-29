# atrium-tessera

Implementation of Tessera, Atrium's content-addressed filesystem.

**Specs** (the authoritative reference; implementation tracks these):
- [tessera-fs](../docs/spec/tessera-fs.md) — on-disk format (normative)
- [tessera-vfs](../docs/spec/tessera-vfs.md) — POSIX mapping (normative)
- [tessera-impl](../docs/spec/tessera-impl.md) — implementation plan (advisory)

## Layout

```
atrium-tessera/
├── core/      tessera-core: C library, freestanding. Codec + algorithms.
├── kmod/      tessera-fs.ko: FreeBSD kernel module (VFS adapter).
├── rs/        Rust crates: tessera-sys (FFI) + tessera (safe) + test-harness.
├── tools/     Userspace binaries: mkfs / fsck / pin / scrub / subvol / etc.
├── tests/     Cross-cutting tests: property / crash / differential / kernel.
└── docs/      Implementation notes.
```

## Build

```sh
# tessera-core (userspace static + shared lib)
cd core && make

# tessera-fs.ko (FreeBSD kernel module; build inside the VM)
cd kmod && make

# Rust crates + tools (cross-compile from host)
cd rs    && cargo build --release --target aarch64-unknown-freebsd
cd tools && cargo build --release --target aarch64-unknown-freebsd
```

## Test

```sh
cd core && make check         # C unit tests
cd rs   && cargo test         # Rust unit + property tests
cd tests/property && cargo test --release
```

## Status

Phase 0 — skeleton in place; algorithm modules are stubs returning `TESSERA_ENOTIMPL`. See [tessera-impl §5](../docs/spec/tessera-impl.md) for the phase plan.
