#!/bin/sh
# Build rs/tessera-decode-c (the Rust on-disk decoders as a host static
# library, D3) and print the linker arguments a C program needs to use it:
#
#   RSLIBS=$(sh scripts/tessera-decode-lib.sh) || exit 1
#   cc -DTESSERA_RUST_DECODE ... prog.c libcore.a $RSLIBS
#
# Used by core-host-tests.sh and core-fuzz.sh (RUST_DECODE=1) and by the
# differential target fuzz/fuzz_diff.c. Build output goes to stderr.
#
# FUZZ_COVERAGE=1 builds the Rust code with SanitizerCoverage (edge counters
# and comparison tracing, as cargo-fuzz does), into its own target dir, so
# libFuzzer is steered by the Rust decoders' branches too and not only by the
# C around them. Link that archive only into -fsanitize=fuzzer binaries.
set -eu
RS="$(cd "$(dirname "$0")/.." && pwd)/atrium-tessera/rs"
cd "$RS"
# --print native-static-libs names the system libraries the Rust std inside
# the archive needs on this host; it is printed only when rustc links, so
# touch the crate root to make sure it does.
touch tessera-decode-c/src/lib.rs
lib="$RS/target/release/libtessera_decode_c.a"
targetarg=""
if [ "${FUZZ_COVERAGE:-0}" = 1 ]; then
    # An explicit --target keeps RUSTFLAGS off build scripts (libc's would
    # otherwise be instrumented and fail to link without the sancov runtime).
    host=$(rustc -vV | sed -n 's/^host: //p')
    targetarg="--target $host"
    export CARGO_TARGET_DIR="$RS/target/fuzzcov"
    lib="$CARGO_TARGET_DIR/$host/release/libtessera_decode_c.a"
    export RUSTFLAGS="-Cpasses=sancov-module -Cllvm-args=-sanitizer-coverage-level=4 \
        -Cllvm-args=-sanitizer-coverage-inline-8bit-counters \
        -Cllvm-args=-sanitizer-coverage-pc-table \
        -Cllvm-args=-sanitizer-coverage-trace-compares -Cdebug-assertions"
fi
# shellcheck disable=SC2086
native=$(cargo rustc -q -p tessera-decode-c --release $targetarg -- --print native-static-libs 2>&1 \
         | sed -n 's/.*native-static-libs: //p' | tail -1)
[ -f "$lib" ] || { echo "tessera-decode-lib: $lib was not built" >&2; exit 1; }
echo "$lib $native"
