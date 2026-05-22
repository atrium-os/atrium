#!/bin/sh
# Build release binaries for every Insula-related
# crate into ./dist/. Companion to test-insula.sh.
#
# Usage:
#   scripts/build-insula.sh           # release build
#   scripts/build-insula.sh --debug   # debug build (faster turnaround)
#
# The dist/ directory after a successful build:
#
#   dist/
#     bin/
#       insula
#       insula-logd
#       vestibulum-macos
#       atrium-netd-macos
#       praeco-macos
#       tabellarius-macos
#       insula-hello
#       atrium-fetch
#       atrium-mon
#       atrium-paint
#       insula-clock
#     include/
#       atrium.h
#     lib/
#       libatrium.dylib   (on macOS; libatrium.so on Linux)
#       libatrium.a
#
# Exits 0 iff every crate builds.

set -u

# (crate, binary-name) pairs. Library-only crates
# without a [[bin]] target (insula-manifest,
# insula-bundle) are skipped — they ship as path-deps
# of the binaries.
BIN_CRATES="
insula-cli:insula
insula-logd:insula-logd
vestibulum-macos:vestibulum-macos
atrium-netd-macos:atrium-netd-macos
praeco-macos:praeco-macos
tabellarius-macos:tabellarius-macos
tabellarius-relay:tabellarius-relay
insula-hello:insula-hello
atrium-fetch:atrium-fetch
atrium-mon:atrium-mon
atrium-paint:atrium-paint
insula-clock:insula-clock
"

MODE=release
for arg in "$@"; do
    case "$arg" in
        --debug)   MODE=debug   ;;
        --release) MODE=release ;;
        -h|--help)
            sed -n '2,28p' "$0"
            exit 0
            ;;
        *)
            printf 'unknown flag: %s\n' "$arg" >&2
            exit 2
            ;;
    esac
done

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 1

if [ "$MODE" = release ]; then
    CARGO_FLAGS="--release"
    TARGET_SUBDIR=release
else
    CARGO_FLAGS=""
    TARGET_SUBDIR=debug
fi

# Fresh dist/ each run so stale artifacts can't sneak
# into a shippable layout.
rm -rf dist
mkdir -p dist/bin dist/lib dist/include

# Detect dylib suffix for the platform.
case "$(uname -s)" in
    Darwin) DYLIB_EXT=dylib ;;
    Linux)  DYLIB_EXT=so    ;;
    FreeBSD) DYLIB_EXT=so   ;;
    *)      DYLIB_EXT=so    ;;
esac

failed=""

# 1. libatrium (cdylib + staticlib).
printf '%-22s ... ' "libatrium"
if cargo build $CARGO_FLAGS --manifest-path libatrium/Cargo.toml --quiet 2>/tmp/build-insula.err; then
    cp "libatrium/target/$TARGET_SUBDIR/libatrium.$DYLIB_EXT" dist/lib/ \
        2>/dev/null || true
    cp "libatrium/target/$TARGET_SUBDIR/libatrium.a" dist/lib/ \
        2>/dev/null || true
    cp libatrium/include/atrium.h dist/include/
    printf 'OK\n'
else
    printf 'FAIL\n'
    cat /tmp/build-insula.err
    failed="$failed libatrium"
fi

# 2. binary crates.
for pair in $BIN_CRATES; do
    crate=${pair%%:*}
    binname=${pair##*:}
    printf '%-22s ... ' "$crate"
    if cargo build $CARGO_FLAGS \
        --manifest-path "$crate/Cargo.toml" --quiet \
        --bin "$binname" 2>/tmp/build-insula.err
    then
        cp "$crate/target/$TARGET_SUBDIR/$binname" dist/bin/
        printf 'OK\n'
    else
        printf 'FAIL\n'
        cat /tmp/build-insula.err
        failed="$failed $crate"
    fi
done

echo
if [ -n "$failed" ]; then
    printf 'failed:%s\n' "$failed" >&2
    exit 1
fi

printf 'dist/ layout:\n'
find dist -type f | sort | sed 's|^|  |'
