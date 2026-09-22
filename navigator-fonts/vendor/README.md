# Vendored crates

## pathfinder_simd 0.5.6 — MIT OR Apache-2.0 (https://github.com/servo/pathfinder)

Pulled in by `allsorts` via `pathfinder_geometry`. Patched (search `ATRIUM PATCH`):

1. `pub mod arm` now honours the `pf-no-simd` feature. Upstream compiles it on every
   nightly aarch64 build, and it calls intrinsics (`simd_minimum_number_nsz`) that the
   repo's pinned nightly does not have, so nothing depending on allsorts could build for
   `aarch64-unknown-freebsd`. `navigator-fonts` enables `pf-no-simd` and uses the scalar
   backend; font conversion has no need for SIMD geometry.
2. Upstream's `cfg` lint noise is allowed, since path dependencies are not lint-capped.

Drop this directory, and the `[patch.crates-io]` entry in `../Cargo.toml`, once upstream
gates the module on the feature.
