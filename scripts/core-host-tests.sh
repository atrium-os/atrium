#!/bin/sh
# Build and run tessera-core's C test suite NATIVELY on the build host.
#
# Why this exists: core/Makefile sets MK_AUTO_OBJ=no so that .OBJDIR ==
# .CURDIR, which is what lets the cross-built libtessera_core.a sit in the
# source tree where TESSERA_CORE_LIB (and the Rust tools) can find it. The
# consequence is that a host build would write its objects to the same paths
# and clobber the cross artifacts — and `bmake check` instead links host test
# objects against the aarch64-FreeBSD archive:
#
#   ld: symbol(s) not found for architecture arm64
#       "_tessera_sha256", referenced from: _main in test_smoke.o
#
# which reads like a missing library and is actually an architecture mismatch.
# So the host build gets its own object directory and its own archive, and
# never touches the cross ones.
#
# No external dependency is needed. BLAKE3 (the default hash since 2026-07-09)
# is vendored in core/src; the legacy SHA-256 arm resolves through hash.c's
# __APPLE__ branch onto CommonCrypto.
#
# usage: scripts/core-host-tests.sh [-k]      -k = keep going after a failure
set -u
CORE="$(cd "$(dirname "$0")/.." && pwd)/atrium-tessera/core"
OBJ="$CORE/build-host"
KEEPGOING=${1:-}

SRCS="error.c hash.c crc.c codec.c cdc.c btree.c manifest.c pack.c journal.c
      extent.c gc.c volume.c quota.c quota_store.c
      b3_blake3.c b3_blake3_portable.c b3_shim.c tessera_reader.c"

# ★ Keep this list derived from the Makefile, not copied by hand. A test added
# there and missed here is a test that silently never runs — the same class of
# bug as the reserve-tree omissions (docs/spec/tessera-reserve-trees.md).
TESTS=$(awk '/^TEST_NAMES=/{f=1;next} f&&/^$/{exit} f{gsub(/\\/,"");print}' \
        "$CORE/Makefile" | tr -d ' \t' | grep -v '^$')
[ -n "$TESTS" ] || { echo "could not read TEST_NAMES from core/Makefile"; exit 2; }

CC=${CC:-cc}
CFLAGS="-O2 -g -fno-strict-aliasing -I$CORE/include -I$CORE/src -Wall"

mkdir -p "$OBJ" || exit 2
echo "== compiling core natively ($(uname -m)) into build-host/ =="
for s in $SRCS; do
    $CC $CFLAGS -c "$CORE/src/$s" -o "$OBJ/${s%.c}.o" || {
        echo "FAILED to compile $s"; exit 1; }
done
ar rcs "$OBJ/libtessera_core.a" "$OBJ"/*.o || exit 1

# Prove the archive is HOST arch before linking anything against it. Getting
# this backwards is the entire reason this script exists.
if file "$OBJ/libtessera_core.a" | grep -qi 'aarch64\|elf'; then
    echo "REFUSING: build-host archive is not host-native — check CC=$CC"
    exit 2
fi
echo "   $(ls -l "$OBJ/libtessera_core.a" | awk '{print $5}') bytes, host-native"

pass=0; fail=0; failed=""
for t in $TESTS; do
    [ -f "$CORE/tests/$t.c" ] || { echo "== $t: NO SOURCE, skipped =="; continue; }
    if ! $CC $CFLAGS -o "$OBJ/$t" "$CORE/tests/$t.c" "$OBJ/libtessera_core.a" \
         2>"$OBJ/$t.buildlog"; then
        echo "== $t: BUILD FAILED =="; sed 's/^/     /' "$OBJ/$t.buildlog" | head -6
        fail=$((fail+1)); failed="$failed $t"
        [ "$KEEPGOING" = "-k" ] || break
        continue
    fi
    if "$OBJ/$t" >"$OBJ/$t.log" 2>&1; then
        echo "== $t: PASS"; pass=$((pass+1))
    else
        echo "== $t: FAIL (exit $?)"; sed 's/^/     /' "$OBJ/$t.log" | tail -8
        fail=$((fail+1)); failed="$failed $t"
        [ "$KEEPGOING" = "-k" ] || break
    fi
done

echo
echo "host tests: $pass passed, $fail failed"
[ $fail -eq 0 ] || { echo "failed:$failed"; exit 1; }
exit 0
