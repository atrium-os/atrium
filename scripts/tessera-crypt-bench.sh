#!/bin/sh
# Tessera encryption E0 (docs/spec/tessera-encryption.md §13): single-core
# throughput of XChaCha20-Poly1305 and XAES-256-GCM on the code paths the
# FreeBSD kernel would use (atrium-tessera/bench/crypt/crypt_bench.c).
#
#   SRC=~/freebsd-src sh scripts/tessera-crypt-bench.sh [seconds-per-case]
#
# SRC is a FreeBSD source tree (only sys/contrib/libsodium is used: the
# kernel's own ChaCha20 "ref", HChaCha20 and poly1305-donna, compiled here
# with the kernel's -O2). OpenSSL's libcrypto (base on FreeBSD, libssl-dev
# on Linux) stands in for the ossl(4) driver's assembly. Runs on FreeBSD,
# Linux and macOS (OPENSSL=prefix for Homebrew's).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${SRC:-/usr/src}"
SOD="$SRC/sys/contrib/libsodium/src/libsodium"
[ -d "$SOD" ] || { echo "no $SOD (set SRC to a FreeBSD source tree)" >&2; exit 2; }
CC="${CC:-cc}"
OUT="${OUT:-$(mktemp -d)}"
SSL_CFLAGS=""; SSL_LIBS="-lcrypto"
if [ -n "${OPENSSL:-}" ]; then
    SSL_CFLAGS="-I$OPENSSL/include"; SSL_LIBS="-L$OPENSSL/lib -lcrypto"
fi
# libsodium's private headers want a few configure results; the kernel build
# passes none either, so the portable ref code is what gets compiled.
cat > "$OUT/sodium-bench.h" <<'H'
#define NATIVE_LITTLE_ENDIAN 1
#define CONFIGURED 1
H
INC="-include $OUT/sodium-bench.h -I$SOD/include/sodium -I$SOD/include"
for f in crypto_core/hchacha20/core_hchacha20.c \
         crypto_stream/chacha20/stream_chacha20.c \
         crypto_stream/chacha20/ref/chacha20_ref.c \
         crypto_onetimeauth/poly1305/onetimeauth_poly1305.c \
         crypto_onetimeauth/poly1305/donna/poly1305_donna.c \
         crypto_verify/sodium/verify.c \
         sodium/utils.c sodium/runtime.c; do
    [ -f "$SOD/$f" ] || continue
    $CC -O2 -w $INC -c "$SOD/$f" -o "$OUT/$(echo "$f" | tr / _).o"
done
# Only the *_keygen helpers reach for randomness; the bench never calls them.
printf '#include <stddef.h>\nvoid randombytes_buf(void *b, size_t n) { (void)b; (void)n; __builtin_trap(); }\nvoid sodium_misuse(void) { __builtin_trap(); }\n' > "$OUT/rb.c"
$CC -O2 -c "$OUT/rb.c" -o "$OUT/rb.o"
$CC -O2 -Wall $INC $SSL_CFLAGS -o "$OUT/crypt_bench" \
    "$ROOT/atrium-tessera/bench/crypt/crypt_bench.c" "$OUT"/*.o $SSL_LIBS
echo "host: $(uname -srm)"
"$OUT/crypt_bench" "${1:-1}"
