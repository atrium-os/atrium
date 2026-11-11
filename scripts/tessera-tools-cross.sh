#!/bin/sh
# Cross-build the Tessera userspace tools (mkfs-tessera, tessera-fsck,
# tessera-stat, ...) for the arm64 FreeBSD guest from a Linux cloud session.
#
#   sh scripts/tessera-tools-cross.sh            # Rust on-disk decoders (default)
#   TESSERA_C_DECODE=1 sh scripts/tessera-tools-cross.sh   # all-C reference
#
# The tools link the C core as a prebuilt archive (tessera-sys build.rs, the
# FreeBSD branch). The Mac builds that archive in bootstrap-atrium.sh's
# corelib phase with bmake; here it is compiled straight from core/Makefile's
# SRCS with clang, against the sysroot scripts/cloud-vm.sh setup pulled from
# the guest (on the Mac, LLVMBIN=$(brew --prefix llvm)/bin and the
# ~/atrium-vm/sysroot the VM setup left). -DTESSERA_RUST_DECODE makes the core's decoders forward to
# tessera-decode (D3, atrium-tessera/docs/rust-decoders.md), which tessera-sys
# always links. Output: atrium-tessera/rs/target/aarch64-unknown-freebsd/release/,
# the default TOOLS= of scripts/tessera-decode-vm-e2e.sh.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CORE="$ROOT/atrium-tessera/core"
RS="$ROOT/atrium-tessera/rs"
SYSROOT="${SYSROOT:-$HOME/atrium-vm/sysroot}"
LLVM="${LLVM:-18}"
# LLVMBIN: a directory with clang, llvm-ar and llvm-nm (default the distro's
# llvm-$LLVM; on the Mac, $(brew --prefix llvm)/bin).
LLVMBIN="${LLVMBIN:-/usr/lib/llvm-$LLVM/bin}"
TRIPLE=aarch64-unknown-freebsd
[ -d "$SYSROOT/usr/include" ] || { echo "no sysroot at $SYSROOT (scripts/cloud-vm.sh setup)" >&2; exit 1; }

DEFS="-D__libtessera_core__"
OUT="$CORE/build-cross-rs"
if [ "${TESSERA_C_DECODE:-}" = 1 ]; then
    OUT="$CORE/build-cross-c"
else
    DEFS="$DEFS -DTESSERA_RUST_DECODE"
fi

# SRCS from the Makefile (backslash-continued), never a copy.
SRCS=$(sed -n '/^SRCS=/,/[^\\]$/p' "$CORE/Makefile" | sed 's/^SRCS=//; s/\\$//' | tr -s ' \t' '\n' | grep '\.c$')
[ -n "$SRCS" ] || { echo "no SRCS in $CORE/Makefile" >&2; exit 1; }

rm -rf "$OUT"
mkdir -p "$OUT"
for f in $SRCS; do
    "$LLVMBIN/clang" --target=aarch64-unknown-freebsd16.0 --sysroot="$SYSROOT" \
        -isystem "$SYSROOT/usr/include" -O2 -fPIC -fno-strict-aliasing \
        -I"$CORE/include" -I"$CORE/src" $DEFS -c "$CORE/src/$f" -o "$OUT/${f%.c}.o"
done
"$LLVMBIN/llvm-ar" rcs "$OUT/libtessera_core.a" "$OUT"/*.o
echo "core: $(ls "$OUT"/*.o | wc -l) objects -> $OUT/libtessera_core.a ($DEFS)"

S="$SYSROOT"
CROSS_RUSTFLAGS="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
(cd "$RS" && TESSERA_CORE_LIB="$OUT" RUSTFLAGS="$CROSS_RUSTFLAGS" \
    cargo +nightly build --quiet --release --target "$TRIPLE" \
    -Z build-std=std,panic_abort -p tessera-tools)

BIN="$RS/target/$TRIPLE/release"
NM="$LLVMBIN/llvm-nm"
for b in mkfs-tessera tessera-fsck; do
    n=$("$NM" "$BIN/$b" 2>/dev/null | grep -c ' [tT] tessera_rs_' || true)
    echo "$b: $n Rust decoder entry points"
    if [ "${TESSERA_C_DECODE:-}" != 1 ] && [ "$n" -eq 0 ]; then
        echo "$b has no Rust decoders" >&2; exit 1
    fi
done
