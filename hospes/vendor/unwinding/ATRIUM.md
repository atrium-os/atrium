# unwinding 0.2.11, patched for Atrium

Upstream: https://github.com/nbdd0121/unwinding (crates.io `unwinding` 0.2.11), MIT OR
Apache-2.0. Used by `hospes-glibc` as the `_Unwind_*` implementation for C++ exceptions
inside foreign libraries (docs/spec/hospes.md §6, H4b). Wired in by `[patch.crates-io]` in
hospes/Cargo.toml.

Two AArch64 fixes, both marked "Atrium patch"; without them nothing built with
`-mbranch-protection=pac-ret` (Ubuntu's arm64 libc++abi, and so Hospes's arm64 H4 fixture)
could be unwound:

1. `src/util.rs` `eh_frame()`, used by every FDE finder: set gimli's CFI vendor to AArch64.
   0.2.11 never sets it, so `DW_CFA_AARCH64_negate_ra_state` (0x2d) fails to parse (it is
   SPARC's `DW_CFA_GNU_window_save` for the default vendor) and phase 1 returns
   `FATAL_PHASE1_ERROR`.
2. `src/unwinder/frame.rs`: the signing state then arrives as a rule for pseudo-register 34,
   which the context cannot index (`unimplemented!()`). Skip it, and when the state is 1
   strip the authentication code from the unwound return address with `xpaclri` (a no-op
   on CPUs without pointer authentication).

To be offered upstream: two patches against 0.2.11 and a draft PR are kept in the project files (linux-compat/unwinding-upstream/); not yet sent.
