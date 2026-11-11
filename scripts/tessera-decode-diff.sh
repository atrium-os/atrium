#!/bin/sh
# Differential test of the Rust on-disk decoders (rs/tessera-decode) against
# the C ones they replace (kernel security review 2026-11-07, D3).
#
# Builds ONE binary with two complete tessera-cores in it: the C core, and the
# core built with -DTESSERA_RUST_DECODE (exactly how tessera_fs.ko is built),
# its global symbols renamed rsmode_* with objcopy. core/fuzz/diff_body.c runs
# every parser once per core over the same input and fuzz_diff.c requires the
# two result traces to be identical. Both cores are compiled with ASAN+UBSAN.
#
#   sh scripts/tessera-decode-diff.sh               # replay every committed corpus
#   sh scripts/tessera-decode-diff.sh -t 600        # ...then explore 600 s with libFuzzer
#   sh scripts/tessera-decode-diff.sh -t 600 -j 4   # ...in 4 parallel jobs
#
# Needs clang with libFuzzer for -t (Linux: clang-18 + libclang-rt-18-dev),
# libmd on Linux, and cargo. Exits nonzero on any difference; libFuzzer leaves
# the input in core/fuzz/artifacts/fuzz_diff-*.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORE="$ROOT/atrium-tessera/core"
OBJ="$CORE/build-diff"
CORPUS="$CORE/fuzz/corpus"
ARTIFACTS="$CORE/fuzz/artifacts"
CC="${CC:-clang-18}"
SECS=0
JOBS=1
while [ $# -gt 0 ]; do
    case "$1" in
        -t) SECS="$2"; shift 2 ;;
        -j) JOBS="$2"; shift 2 ;;
        *)  echo "usage: $0 [-t seconds] [-j jobs]"; exit 2 ;;
    esac
done

SRCS="error.c hash.c crc.c codec.c cdc.c btree.c manifest.c pack.c journal.c
      extent.c gc.c volume.c quota.c quota_store.c
      b3_blake3.c b3_blake3_portable.c b3_shim.c tessera_reader.c"
LIBS="${FUZZ_LIBS-}"
[ -z "${FUZZ_LIBS+x}" ] && [ "$(uname)" = Linux ] && LIBS="-lmd"
RSLIBS=$(sh "$ROOT/scripts/tessera-decode-lib.sh") || exit 2

FLAGS="-g -O1 -fno-strict-aliasing -Wall -fno-omit-frame-pointer \
       -fsanitize=address,undefined -fno-sanitize-recover=all \
       -I$CORE/include -I$CORE/src"
[ "$SECS" -gt 0 ] && FLAGS="$FLAGS -fsanitize=fuzzer-no-link"

rm -rf "$OBJ"; mkdir -p "$OBJ/c" "$OBJ/rs" "$ARTIFACTS" || exit 2

# -fno-sanitize-address-globals-dead-stripping: otherwise ASAN puts each
# instrumented global in a COMDAT group whose name is the same in both builds
# of a file, and the linker keeps one build's globals and drops the other's.
FLAGS="$FLAGS -fno-sanitize-address-globals-dead-stripping"
# shellcheck disable=SC2086
for s in $SRCS; do
    $CC $FLAGS -c "$CORE/src/$s" -o "$OBJ/c/${s%.c}.o" || exit 1
    $CC $FLAGS -DTESSERA_RUST_DECODE -c "$CORE/src/$s" -o "$OBJ/rs/${s%.c}.o" || exit 1
done

# Rename every global the Rust-mode core DEFINES (its tessera_* API, BLAKE3,
# ...). References to the tessera_rs_* exports and to libc stay as they are.
nm -g --defined-only "$OBJ"/rs/*.o | awk 'NF==3 && $2 ~ /[TDBRVW]/ {print $3}' \
    | sort -u > "$OBJ/rs.syms"
awk '{print $1 " rsmode_" $1}' "$OBJ/rs.syms" > "$OBJ/rs.map"
awk '{print "#define " $1 " rsmode_" $1}' "$OBJ/rs.syms" > "$OBJ/rsnames.h"
for o in "$OBJ"/rs/*.o; do objcopy --redefine-syms="$OBJ/rs.map" "$o" || exit 1; done
ar rcs "$OBJ/libcore_c.a" "$OBJ"/c/*.o
ar rcs "$OBJ/libcore_rs.a" "$OBJ"/rs/*.o
echo "tessera-decode-diff: C core + Rust-decoder core ($(wc -l < "$OBJ/rs.syms") symbols renamed)"

# shellcheck disable=SC2086
$CC $FLAGS -DDIFF_SIDE=c -c "$CORE/fuzz/diff_body.c" -o "$OBJ/body_c.o" || exit 1
# shellcheck disable=SC2086
$CC $FLAGS -DDIFF_SIDE=rs -include "$OBJ/rsnames.h" \
    -c "$CORE/fuzz/diff_body.c" -o "$OBJ/body_rs.o" || exit 1

link() { # out extra-flags...
    _out="$1"; shift
    # shellcheck disable=SC2086
    $CC $FLAGS "$@" -o "$_out" "$CORE/fuzz/fuzz_diff.c" "$OBJ/body_c.o" "$OBJ/body_rs.o" \
        "$OBJ/libcore_c.a" "$OBJ/libcore_rs.a" $RSLIBS $LIBS
}
link "$OBJ/fuzz_diff.replay" -DDIFF_REPLAY_MAIN || exit 1

# ── replay: every committed corpus, in its own target's shape ──────────────
export ASAN_OPTIONS=detect_leaks=0:abort_on_error=1 UBSAN_OPTIONS=print_stacktrace=1
rc=0
# Log to a file and check the binary's own status: `replay ... | tail` would
# report tail's status, and a difference found in replay used to pass.
replay() {
    "$OBJ/fuzz_diff.replay" "$@" > "$OBJ/replay.log" 2>&1 || rc=1
    grep -v "load_node" "$OBJ/replay.log" | tail -3
}
for t in manifest pack btree journal superblock; do
    set -- "$CORPUS/fuzz_$t"/*
    [ -f "$1" ] || continue
    printf '== diff %-10s ' "$t:"
    replay "$t" "$@"
done
# The codec target has no corpus of its own: use every file of every corpus
# (and the fuzz_diff corpus when there is one).
printf '== diff %-10s ' "codec:"
replay codec "$CORPUS"/fuzz_*/*
# btree_ops grows its own tree; any bytes seed it. Every corpus file is one
# operation stream (geometry, key space, length and damage from its bytes).
printf '== diff %-10s ' "btree_ops:"
replay btree_ops "$CORPUS"/fuzz_*/*
if [ -d "$CORPUS/fuzz_diff" ] && [ -n "$(ls "$CORPUS/fuzz_diff")" ]; then
    printf '== diff %-10s ' "auto:"
    replay auto "$CORPUS"/fuzz_diff/*
fi
[ $rc -eq 0 ] || { echo "tessera-decode-diff: DIFFERENCES FOUND"; exit 1; }

# ── explore ─────────────────────────────────────────────────────────────────
[ "$SECS" -gt 0 ] || exit 0
# The fuzzing binary links a Rust archive built with SanitizerCoverage, so
# libFuzzer also sees the Rust decoders' branches and comparisons.
RSLIBS=$(FUZZ_COVERAGE=1 sh "$ROOT/scripts/tessera-decode-lib.sh") || exit 2
link "$OBJ/fuzz_diff" -fsanitize=fuzzer || exit 1
SEED="$OBJ/seed"; mkdir -p "$SEED" "$CORPUS/fuzz_diff"
# Seed with every target's corpus, prefixed by that target's selector byte.
i=0
for t in codec manifest pack btree journal superblock btree_ops; do
    src="$t"; [ "$t" = codec ] && src=superblock; [ "$t" = btree_ops ] && src=btree
    for f in "$CORPUS/fuzz_$src"/*; do
        [ -f "$f" ] || continue
        { printf "\\$(printf %03o $i)"; cat "$f"; } > "$SEED/$t-$(basename "$f")"
    done
    i=$((i+1))
done
echo "== explore fuzz_diff ${SECS}s x $JOBS job(s) =="
"$OBJ/fuzz_diff" "$CORPUS/fuzz_diff" "$SEED" -max_total_time="$SECS" -max_len=40000 \
    -timeout=30 -use_value_profile=1 -print_final_stats=1 -jobs="$JOBS" -workers="$JOBS" \
    -artifact_prefix="$ARTIFACTS/fuzz_diff-" > "$OBJ/explore.log" 2>&1
st=$?
cat "$OBJ"/fuzz-*.log 2>/dev/null >> "$OBJ/explore.log"
grep -aE "DIFFER|LAYOUT|ERROR|runtime error|stat::number_of_executed_units|DONE" "$OBJ/explore.log" | tail -12
[ $st -eq 0 ] || { echo "tessera-decode-diff: EXPLORE FOUND A DIFFERENCE OR DEFECT (see $ARTIFACTS)"; exit 1; }
echo "tessera-decode-diff: explore clean"
