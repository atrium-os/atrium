#!/bin/sh
# Fuzz tessera-core's on-disk parsers, in two phases.
#
# ★ WHY TWO PHASES. No single compiler on this host can do fuzzing AND ASAN:
#
#   - Apple clang has ASAN but NO libFuzzer runtime (libclang_rt.fuzzer_osx.a
#     is absent), so -fsanitize=fuzzer fails to LINK.
#   - Homebrew LLVM 21 has libFuzzer, but its ASAN runtime DEADLOCKS before
#     main() on Darwin 25.5 — ASAN's malloc interceptor re-enters ASAN's own
#     initialization and spins forever in StaticSpinMutex::LockSlow. Measured:
#     -fsanitize=fuzzer and fuzzer,undefined run; adding `address` hangs.
#     (Full stack in fuzz/replay_main.c.)
#
# So:
#   EXPLORE  Homebrew clang, -fsanitize=fuzzer,undefined — grows the corpus,
#            catches UB, hangs and assertion failures. Needs brew.
#   REPLAY   Apple clang, -fsanitize=address,undefined, over that corpus via a
#            plain main() (fuzz/replay_main.c). Catches memory errors. Needs
#            NOTHING beyond the default toolchain.
#
# The corpus is committed, so REPLAY is a deterministic regression test that
# runs anywhere. That is the half worth wiring into the normal test suite;
# EXPLORE stays on demand.
#
# usage: scripts/core-fuzz.sh [-t seconds] [--replay-only] [target ...]
#          -t seconds     explore time per target (default 60)
#          --replay-only  skip exploration; just replay the committed corpus
#                         under ASAN. This is the mode with no brew dependency.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORE="$ROOT/atrium-tessera/core"
OBJ="$CORE/build-fuzz"
CORPUS="$CORE/fuzz/corpus"
ARTIFACTS="$CORE/fuzz/artifacts"

SECS=60
REPLAY_ONLY=0
TARGETS=""
while [ $# -gt 0 ]; do
    case "$1" in
        -t)            SECS="$2"; shift 2 ;;
        --replay-only) REPLAY_ONLY=1; shift ;;
        -*)            echo "unknown flag: $1"; exit 2 ;;
        *)             TARGETS="$TARGETS $1"; shift ;;
    esac
done
[ -n "$TARGETS" ] || TARGETS="fuzz_manifest fuzz_pack fuzz_btree fuzz_superblock fuzz_journal"

SRCS="error.c hash.c crc.c codec.c cdc.c btree.c manifest.c pack.c journal.c
      extent.c gc.c volume.c quota.c quota_store.c
      b3_blake3.c b3_blake3_portable.c b3_shim.c tessera_reader.c"

INC="-I$CORE/include -I$CORE/src"
mkdir -p "$OBJ" "$ARTIFACTS" || exit 2

# build_core <objdir> <cc> <cflags...>
build_core() {
    _o="$1"; _cc="$2"; shift 2
    rm -rf "$_o"; mkdir -p "$_o" || return 2
    for s in $SRCS; do
        # shellcheck disable=SC2086
        $_cc "$@" $INC -c "$CORE/src/$s" -o "$_o/${s%.c}.o" || {
            echo "FAILED to compile $s"; return 1; }
    done
    ar rcs "$_o/libcore.a" "$_o"/*.o
}

rc=0

# ── Phase 1: EXPLORE ────────────────────────────────────────────────────────
if [ "$REPLAY_ONLY" = 0 ]; then
    # Find a clang that can LINK AND RUN a fuzzer. Probing the flag alone is
    # not enough twice over: Apple clang fails at LINK time (missing runtime),
    # and the Homebrew ASAN combination fails at RUN time (startup deadlock).
    # Only actually running a probe binary distinguishes all three outcomes.
    FZCC=""
    for c in ${FUZZ_CC:-} /opt/homebrew/opt/llvm/bin/clang /usr/local/opt/llvm/bin/clang; do
        [ -n "$c" ] && [ -x "$c" ] || continue
        cat > "$OBJ/probe.c" <<'EOF'
#include <stdint.h>
#include <stddef.h>
int LLVMFuzzerTestOneInput(const uint8_t *d, size_t s) { (void)d; (void)s; return 0; }
EOF
        "$c" -fsanitize=fuzzer,undefined "$OBJ/probe.c" -o "$OBJ/probe" 2>/dev/null || continue
        # ★ Bound the probe. The failure we are screening for is a HANG, and an
        # unbounded probe would inherit it — the check would never fail, it
        # would just never finish.
        "$OBJ/probe" -runs=1 >/dev/null 2>&1 &
        _p=$!; _ok=0
        for _i in 1 2 3 4 5 6 7 8 9 10; do
            kill -0 $_p 2>/dev/null || { _ok=1; break; }
            sleep 1
        done
        kill -9 $_p 2>/dev/null
        [ "$_ok" = 1 ] && { FZCC="$c"; break; }
        echo "core-fuzz: $c builds a fuzzer but it HANGS at startup — skipping"
    done
    rm -f "$OBJ/probe" "$OBJ/probe.c"

    if [ -z "$FZCC" ]; then
        echo "core-fuzz: no usable fuzzing compiler; EXPLORE skipped."
        echo "  Apple clang has no libFuzzer runtime. Install one: brew install llvm"
        echo "  Running REPLAY over the committed corpus instead."
        REPLAY_ONLY=1
    else
        echo "core-fuzz: EXPLORE with $FZCC"
        # UBSAN only — see the header. -fsanitize=fuzzer-no-link on the library
        # so every core object gets coverage instrumentation.
        FZFLAGS="-g -O1 -fno-strict-aliasing -Wall \
                 -fsanitize=fuzzer-no-link,undefined -fno-sanitize-recover=all \
                 -fno-omit-frame-pointer"
        # shellcheck disable=SC2086
        build_core "$OBJ/explore" "$FZCC" $FZFLAGS || exit 1

        for t in $TARGETS; do
            [ -f "$CORE/fuzz/$t.c" ] || { echo "no such target: $t"; exit 2; }
            # shellcheck disable=SC2086
            $FZCC $FZFLAGS $INC -fsanitize=fuzzer -o "$OBJ/$t" \
                "$CORE/fuzz/$t.c" "$OBJ/explore/libcore.a" || exit 1
            mkdir -p "$CORPUS/$t"
            # ★ libFuzzer's default -max_len is 4096. tessera_pack_open()
            # rejects anything shorter than two 4096-B sectors, so at the
            # default EVERY input died on the first `if` — measured: 27.8
            # MILLION executions at cov:2, a fuzzer that ran perfectly and
            # tested nothing. Any target with a minimum input size must raise
            # this explicitly. Check `lim:` in the log against the parser's
            # minimum before believing a run.
            # Each target's minimum viable input, NOT a guess:
            #   fuzz_pack        2 sectors (header + footer)
            #   fuzz_btree       2 control bytes + >=1 sector, more = deeper trees
            #   fuzz_superblock  1 control byte + the two SB sectors
            case "$t" in
                fuzz_pack)       MAXLEN=32768 ;;
                fuzz_btree)      MAXLEN=32768 ;;
                fuzz_superblock) MAXLEN=16384 ;;
                fuzz_journal)    MAXLEN=8192 ;;
                *)               MAXLEN=4096 ;;
            esac
            echo
            echo "== EXPLORE $t: ${SECS}s (max_len=$MAXLEN) =="
            # -use_value_profile: pack_open gates on `total_pack_bytes == len`,
            # an 8-byte compare a blind mutator will never satisfy. Value
            # profile turns partial-compare progress into coverage, which is
            # what gets the fuzzer through that gate and into the reader.
            #
            # ★ Log to a FILE and tail the file. Piping into `tail` would make
            # $? the status of tail — 0 whether or not the fuzzer crashed.
            UBSAN_OPTIONS=print_stacktrace=1 \
            # ★ -timeout: libFuzzer's default is 1200s per input. Journal
            # replay loops on values read off disk, so a non-terminating input
            # is a REAL possible finding — and at the default the fuzzer would
            # hang for 20 minutes rather than report it. 20s is far above any
            # legitimate input here.
            "$OBJ/$t" "$CORPUS/$t" -max_total_time="$SECS" -max_len="$MAXLEN" \
                -timeout=20 -use_value_profile=1 -print_final_stats=1 \
                -artifact_prefix="$ARTIFACTS/$t-" > "$OBJ/$t.explore.log" 2>&1
            st=$?
            grep -aE "^#[0-9]+.*(DONE|cov:)|ERROR|runtime error|stat::" \
                "$OBJ/$t.explore.log" | tail -8
            if [ $st -ne 0 ]; then
                echo ">> $t: EXPLORE FOUND A DEFECT — reproducer in $ARTIFACTS/"
                rc=1
            fi
        done
    fi
fi

# ── Phase 2: REPLAY under ASAN ──────────────────────────────────────────────
CC=${CC:-cc}
echo
echo "core-fuzz: REPLAY under ASAN+UBSAN with $CC"
SANFLAGS="-g -O1 -fno-strict-aliasing -Wall \
          -fsanitize=address,undefined -fno-sanitize-recover=all \
          -fno-omit-frame-pointer"
# shellcheck disable=SC2086
build_core "$OBJ/replay" "$CC" $SANFLAGS || exit 1

for t in $TARGETS; do
    [ -f "$CORE/fuzz/$t.c" ] || continue
    # shellcheck disable=SC2086
    $CC $SANFLAGS $INC -o "$OBJ/$t.replay" "$CORE/fuzz/$t.c" \
        "$CORE/fuzz/replay_main.c" "$OBJ/replay/libcore.a" || exit 1

    set -- "$CORPUS/$t"/* "$ARTIFACTS/$t-"*
    files=""
    for f in "$@"; do [ -f "$f" ] && files="$files $f"; done
    if [ -z "$files" ]; then
        echo "== REPLAY $t: no corpus yet — nothing to replay =="
        continue
    fi
    # shellcheck disable=SC2086
    if env ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 \
           UBSAN_OPTIONS=print_stacktrace=1 \
           "$OBJ/$t.replay" $files > "$OBJ/$t.replay.log" 2>&1; then
        echo "== REPLAY $t: $(tail -1 "$OBJ/$t.replay.log") — clean"
    else
        echo "== REPLAY $t: DEFECT =="
        tail -20 "$OBJ/$t.replay.log"
        rc=1
    fi
done

echo
[ $rc -eq 0 ] && echo "core-fuzz: clean" || echo "core-fuzz: DEFECTS FOUND"
exit $rc
