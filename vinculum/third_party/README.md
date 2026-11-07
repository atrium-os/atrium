# Vendored crates

## pdl-compiler 0.5.2 (patched)

Google's PDL compiler (https://github.com/google/pdl, Apache-2.0, see
`pdl-compiler/LICENSE`), used by `vinculum-hci-proto/build.rs` to generate
packet parsers. Vendored from crates.io 0.5.2 with its `tests/` and `scripts/`
removed, and one fix in `src/backends/rust/decoder.rs`:

- **Custom fields were read with no length check.** A typedef field of a
  `custom_field` type (HCI's 48-bit `Address`) decoded with
  `buf.get_uint_le(6)` without checking that 6 bytes remained, so a short
  packet panicked inside `bytes` instead of returning `InvalidLengthError`.
  Found by `vinculum-hci-proto/tests/trace.rs` (`mutations_do_not_panic`),
  first in `SimplePairingComplete`. Every custom-field read now checks
  `remaining()` first.

To report upstream (google/pdl). Drop the vendored copy when a release
carries the fix. Wired in with `[patch.crates-io]` in `vinculum/Cargo.toml`.
