#!/bin/sh
# Static checks on the Rust on-disk decoders (rs/tessera-decode, D3):
#
#   1. cargo test (unit tests);
#   2. compile the crate for the two kernel targets (aarch64 and x86_64,
#      no_std, opt-level 3, the ffi exports included) and require that the
#      object references NOTHING but memcpy/memmove/memcmp/memset: in
#      particular no core::panicking or slice-index-fail routine, so no input
#      can make the decoders panic the kernel;
#   3. report the largest stack frame (kmod audit T2: the C decoders kept up to
#      4160 bytes on a 16 KiB kernel stack) and fail above MAX_FRAME bytes.
#
# The differential test against the C decoders is tessera-decode-diff.sh.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RS="$ROOT/atrium-tessera/rs"
OUT="${TMPDIR:-/tmp}/tessera-decode-check.$$"
MAX_FRAME="${MAX_FRAME:-512}"
OBJDUMP="${OBJDUMP:-$(command -v llvm-objdump || ls /usr/lib/llvm-*/bin/llvm-objdump 2>/dev/null | tail -1)}"
NM="${NM:-$(command -v llvm-nm || ls /usr/lib/llvm-*/bin/llvm-nm 2>/dev/null | tail -1)}"
mkdir -p "$OUT"
trap 'rm -f "$OUT"/*.o; rmdir "$OUT"' EXIT

cd "$RS"
echo "== cargo test -p tessera-decode"
cargo test -q -p tessera-decode --features ffi 2>&1 | grep -E "test result|FAILED|panicked" || true
cargo test -q -p tessera-decode --features ffi >/dev/null 2>&1 || { echo "cargo test FAILED"; exit 1; }

rc=0
for target in aarch64-unknown-none-softfloat x86_64-unknown-none; do
    o="$OUT/$target.o"
    rustup target add "$target" >/dev/null 2>&1 || true
    # No LTO, so the object is machine code rather than bitcode; build-std
    # core only (the tree's .cargo/config.toml asks for std).
    CARGO_PROFILE_RELEASE_LTO=false cargo rustc -q -Zbuild-std=core --release \
        --target "$target" -p tessera-decode --features ffi -- \
        --emit=obj="$o" -C embed-bitcode=no 2>/dev/null
    undef=$("$NM" -u "$o" | awk '{print $2}' | sort -u | tr '\n' ' ')
    bad=$(echo "$undef" | tr ' ' '\n' | grep -vE '^(memcpy|memmove|memcmp|memset|bcmp)?$' || true)
    exports=$("$NM" --defined-only "$o" | grep -c ' T tessera_rs_' || true)
    # Stack adjustments in the prologues, as "<bytes> <function>" (hex -> dec
    # in the shell: mawk has no strtonum).
    case "$target" in
        aarch64*) pat='/sub[ \t]+sp, sp, #/ { n = split($0, a, "#"); print a[n], f }' ;;
        *)        pat='/sub[a-z]*[ \t]+\$0x[0-9a-f]+, *%rsp/ { match($0, /\$0x[0-9a-f]+/);
                        print substr($0, RSTART + 1, RLENGTH - 1), f }' ;;
    esac
    frames=$("$OBJDUMP" -d --no-show-raw-insn "$o" | awk '/^[0-9a-f]+ <.*>:/ { f = $2 } '"$pat" \
             | while read -r h f; do echo "$((h)) $f"; done)
    top=$(echo "$frames" | sort -rn | head -1)
    echo "== $target: $exports exports; external references: ${undef:-none}; largest frame: ${top:-none}"
    if [ -n "$bad" ]; then
        echo "   FAIL: references outside mem*: $bad (a panic path?)"; rc=1
    fi
    sz=$(echo "$top" | awk '{print $1+0}')
    if [ "${sz:-0}" -gt "$MAX_FRAME" ]; then
        echo "   FAIL: stack frame over $MAX_FRAME bytes"; rc=1
    fi
done
[ $rc -eq 0 ] && echo "tessera-decode-check: OK" || echo "tessera-decode-check: FAILED"
exit $rc
