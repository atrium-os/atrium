#!/bin/sh
# Instruction cost of a B-tree lookup through the Tessera core, four ways
# (D3, atrium-tessera/docs/rust-decoders.md "Cross-language LTO"):
#
#   c    the C decoders
#   rs   the Rust decoders as a static library (the normal D3 build)
#   lto  the Rust decoders with cross-language LTO: C and Rust both as LLVM
#        bitcode, optimised together at link time, so decoders inline into C
#   clto the C decoders with LTO too, so "lto vs clto" is the like-for-like
#        figure (LTO also inlines C across files)
#
#   sh scripts/tessera-lto-bench.sh [keys] [lookups]     # default 20000 100000
#
# Counts instructions with callgrind (host, deterministic) and prints a wall
# time per lookup, which is only indicative on a shared host; kernel timing
# is a separate run on real hardware. All four variants use the same clang,
# -O2, and the same bench (atrium-tessera/core/bench/btree_lookup.c).
#
# LTO needs clang, ld.lld and rustc on ONE LLVM version. CLANG defaults to
# clang-<N> where N is rustc's LLVM major; the linker is the rust-lld that
# ships with that rustc (its gcc-ld/ld.lld wrapper).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORE="$ROOT/atrium-tessera/core"
RS="$ROOT/atrium-tessera/rs"
KEYS="${1:-20000}"; LOOKUPS="${2:-100000}"
OUT="${BENCH_OUT:-$CORE/build-bench}"

llvm=$(cd "$RS" && rustc -vV | sed -n 's/^LLVM version: \([0-9]*\).*/\1/p')
CLANG="${CLANG:-clang-$llvm}"
have=$("$CLANG" --version | sed -n '1s/.*clang version \([0-9]*\).*/\1/p')
[ "$have" = "$llvm" ] || { echo "rustc is LLVM $llvm but $CLANG is clang $have (set CLANG=)" >&2; exit 1; }
LLD="$(cd "$RS" && rustc --print sysroot)/lib/rustlib/$(cd "$RS" && rustc -vV | sed -n 's/^host: //p')/bin/gcc-ld/ld.lld"
[ -x "$LLD" ] || { echo "no $LLD" >&2; exit 1; }
command -v valgrind >/dev/null || { echo "needs valgrind" >&2; exit 1; }

SRCS="error.c hash.c crc.c codec.c cdc.c btree.c manifest.c pack.c journal.c
      extent.c gc.c volume.c quota.c quota_store.c
      b3_blake3.c b3_blake3_portable.c b3_shim.c b3_blake3_sse41.c b3_blake3_avx2.c
      tessera_reader.c"
LIBS=""
[ "$(uname)" = Linux ] && LIBS="-lmd"

# rust_lib <target-dir> [rustflags]: build tessera-decode-c, print the archive
rust_lib() {
    (cd "$RS" && CARGO_TARGET_DIR="$1" RUSTFLAGS="${2:-}" CARGO_PROFILE_RELEASE_LTO="${3:-thin}" \
        cargo build -q -p tessera-decode-c --release >&2)
    echo "$1/release/libtessera_decode_c.a"
}

# variant <name> <cflags> <archive or ""> <ldflags>
variant() {
    d="$OUT/$1"; mkdir -p "$d"
    for s in $SRCS; do
        # shellcheck disable=SC2086
        "$CLANG" -O2 -g -fno-strict-aliasing -I"$CORE/include" -I"$CORE/src" $2 \
            -c "$CORE/src/$s" -o "$d/${s%.c}.o"
    done
    # shellcheck disable=SC2086
    "$CLANG" -O2 -g -I"$CORE/include" $2 $4 -o "$d/bench" "$CORE/bench/btree_lookup.c" \
        "$d"/*.o $3 $LIBS -lpthread -ldl
    wall=$("$d/bench" "$KEYS" $((LOOKUPS * 20)) | sed -n 's/.*best \([0-9.]*\) ns.*/\1/p')
    valgrind --tool=callgrind --callgrind-out-file="$d/cg.out" "$d/bench" "$KEYS" "$LOOKUPS" \
        >/dev/null 2>"$d/cg.log"
    ir=$(sed -n 's/.*Collected : \([0-9]*\).*/\1/p' "$d/cg.log")
    printf "%-5s %14s instructions  %6s ns/lookup (wall, indicative)\n" "$1" "$ir" "$wall"
    eval "IR_$1=$ir"
}

echo "== $KEYS keys, $LOOKUPS lookups; $CLANG, rust-lld, rustc LLVM $llvm" >&2
RSLIB=$(rust_lib "$OUT/target-rs")
LTOLIB=$(rust_lib "$OUT/target-lto" "-Clinker-plugin-lto" false)
variant c   "" "" ""
variant rs  "-DTESSERA_RUST_DECODE" "$RSLIB" ""
variant lto "-DTESSERA_RUST_DECODE -flto=thin" "$LTOLIB" "-flto=thin -fuse-ld=lld --ld-path=$LLD"
variant clto "-flto=thin" "" "-flto=thin -fuse-ld=lld --ld-path=$LLD"
# Inlined? The LTO binary should have (almost) no calls into tessera_rs_*.
n=$(objdump -d "$OUT/lto/bench" | grep -c 'call.*<tessera_rs_' || true)
echo "lto: $n direct calls to tessera_rs_* left"
awk -v c="$IR_c" -v r="$IR_rs" -v l="$IR_lto" -v cl="$IR_clto" 'BEGIN {
    printf "rs vs c: %+.1f%%   lto vs clto: %+.1f%%   (whole run incl. building the tree)\n",
        (r-c)*100/c, (l-cl)*100/cl }'
