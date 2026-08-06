#!/bin/sh
# Build and run tessera-core's C test suite NATIVELY on the build host — twice:
# once plain, once under AddressSanitizer + UndefinedBehaviorSanitizer.
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
# So each host build gets its own object directory and its own archive, and
# never touches the cross ones.
#
# ★ WHY SANITIZERS RUN HERE AND NOWHERE ELSE. The core is the only part of
# Tessera that can run in a normal process: the kmod's 25k-line VFS binding is
# bound to FreeBSD's vnode/buffer-cache/taskqueue machinery and can only be
# exercised in the VM, where ASAN is not available. So this is the one place a
# use-after-free or an out-of-bounds read in the btree/manifest/pack/journal
# code gets caught by a tool instead of by a corrupted volume days later. It
# costs a few seconds and it runs on every build (bootstrap ALWAYS_PHASES).
#
# No external dependency is needed. BLAKE3 (the default hash since 2026-07-09)
# is vendored in core/src; the legacy SHA-256 arm resolves through hash.c's
# __APPLE__ branch onto CommonCrypto.
#
# usage: scripts/core-host-tests.sh [-k] [--no-san]
#          -k        keep going after a failure
#          --no-san  plain pass only (for a fast inner loop; CI should not)
set -u
CORE="$(cd "$(dirname "$0")/.." && pwd)/atrium-tessera/core"

KEEPGOING=""
WANT_SAN=1
for a in "$@"; do
    case "$a" in
        -k)        KEEPGOING="-k" ;;
        --no-san)  WANT_SAN=0 ;;
        *)         echo "unknown arg: $a"; exit 2 ;;
    esac
done

# NOTE: hand-copied, and it should not be — tessera-sys/build.rs now parses the
# same list out of core/Makefile (parse_make_srcs) for its host compile. Worth
# collapsing this onto that single source, for the reason the TESTS comment
# below already gives: a file added to the Makefile and missed here silently
# stops being built.
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
BASE_CFLAGS="-g -fno-strict-aliasing -I$CORE/include -I$CORE/src -Wall"

# -fno-sanitize-recover: UBSAN's default is to PRINT and CARRY ON, which means a
# test still exits 0 and the finding scrolls past. Abort instead, so an
# undefined-behaviour hit fails the suite like any other failure.
SAN_CFLAGS="-O1 -fsanitize=address,undefined -fno-sanitize-recover=all \
            -fno-omit-frame-pointer"
# LeakSanitizer is not supported on Darwin/arm64 — asking for it makes ASAN
# abort at startup with "detect_leaks is not supported on this platform", which
# looks exactly like a test failure. Leaks in short-lived test processes are not
# what this pass is for anyway; the target is memory ERRORS.
SAN_RUN_ENV="ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 \
             UBSAN_OPTIONS=print_stacktrace=1"

total_pass=0; total_fail=0; total_failed=""

# run_suite <label> <objdir> <extra-cflags>
run_suite() {
    label="$1"; obj="$2"; extra="$3"
    cflags="$BASE_CFLAGS $extra"
    rm -rf "$obj"; mkdir -p "$obj" || return 2

    echo "== [$label] compiling core natively ($(uname -m)) into $(basename "$obj")/ =="
    for s in $SRCS; do
        # shellcheck disable=SC2086
        $CC $cflags -c "$CORE/src/$s" -o "$obj/${s%.c}.o" || {
            echo "FAILED to compile $s"; return 1; }
    done
    ar rcs "$obj/libtessera_core.a" "$obj"/*.o || return 1

    # Prove the archive is HOST arch before linking anything against it. Getting
    # this backwards is the entire reason this script exists.
    if file "$obj/libtessera_core.a" | grep -qi 'aarch64\|elf'; then
        echo "REFUSING: $label archive is not host-native — check CC=$CC"
        return 2
    fi
    echo "   $(ls -l "$obj/libtessera_core.a" | awk '{print $5}') bytes, host-native"

    pass=0; fail=0
    for t in $TESTS; do
        [ -f "$CORE/tests/$t.c" ] || { echo "== $t: NO SOURCE, skipped =="; continue; }
        # shellcheck disable=SC2086
        if ! $CC $cflags -o "$obj/$t" "$CORE/tests/$t.c" "$obj/libtessera_core.a" \
             2>"$obj/$t.buildlog"; then
            echo "== [$label] $t: BUILD FAILED =="
            sed 's/^/     /' "$obj/$t.buildlog" | head -6
            fail=$((fail+1)); total_failed="$total_failed $label/$t"
            [ "$KEEPGOING" = "-k" ] || break
            continue
        fi
        if env $SAN_RUN_ENV "$obj/$t" >"$obj/$t.log" 2>&1; then
            echo "== [$label] $t: PASS"; pass=$((pass+1))
        else
            rc=$?
            echo "== [$label] $t: FAIL (exit $rc)"
            # A sanitizer report is the interesting part of the log and it is at
            # the END, after the test's own chatter — show the tail, and lift the
            # summary line out so it is not lost in the noise.
            sed 's/^/     /' "$obj/$t.log" | tail -12
            grep -aE "ERROR: (Address|Undefined)|runtime error:" "$obj/$t.log" \
                | head -3 | sed 's/^/  >> /'
            fail=$((fail+1)); total_failed="$total_failed $label/$t"
            [ "$KEEPGOING" = "-k" ] || break
        fi
    done
    echo "   [$label] $pass passed, $fail failed"
    total_pass=$((total_pass+pass)); total_fail=$((total_fail+fail))
    return 0
}

run_suite plain "$CORE/build-host" "-O2" || exit $?

if [ "$WANT_SAN" = 1 ]; then
    # ★ Probe with a REAL program. The first version of this compiled empty
    # stdin, which has no main and so fails to link on any compiler — a check
    # that could never pass, which then fell through to the warning below and
    # skipped the sanitizer pass permanently while the suite still said
    # "16 passed". A capability probe that cannot succeed is worse than no
    # probe: it disables the thing it is guarding and reports success.
    if echo 'int main(void){return 0;}' \
       | $CC -fsanitize=address,undefined -x c -o /dev/null - 2>/dev/null; then
        echo
        run_suite asan+ubsan "$CORE/build-host-san" "$SAN_CFLAGS" || exit $?
    else
        echo
        echo "WARNING: $CC cannot build with -fsanitize=address,undefined — sanitizer pass SKIPPED"
        echo "         (this is the pass that catches use-after-free / OOB in the core)"
    fi
fi

# ── Fuzz-corpus replay ──────────────────────────────────────────────────────
# Replay the committed fuzz corpus (atrium-tessera/core/fuzz/corpus) through
# the parsers under ASAN+UBSAN. ~3s, and it needs no special toolchain — the
# corpus was GENERATED by libFuzzer on a machine that had one, but replaying it
# is just a plain main() over files (fuzz/replay_main.c).
#
# This is the half of fuzzing that belongs in every build. Exploration finds
# new inputs and needs brew LLVM; replay turns every input it ever found into a
# permanent regression test. #124's out-of-bounds pack geometry is in there.
if [ "$WANT_SAN" = 1 ] && [ -d "$CORE/fuzz/corpus" ]; then
    echo
    if sh "$(dirname "$0")/core-fuzz.sh" --replay-only > "$CORE/build-host/replay.log" 2>&1; then
        grep -E "^== REPLAY" "$CORE/build-host/replay.log" | sed 's/^/   /'
    else
        echo "FUZZ REPLAY FAILED:"
        tail -25 "$CORE/build-host/replay.log" | sed 's/^/     /'
        total_fail=$((total_fail+1)); total_failed="$total_failed fuzz-replay"
    fi
fi

echo
echo "host tests: $total_pass passed, $total_fail failed"
[ $total_fail -eq 0 ] || { echo "failed:$total_failed"; exit 1; }
exit 0
