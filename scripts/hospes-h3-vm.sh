#!/bin/sh
# Hospes H3 in a FreeBSD VM (cloud-vm.sh): cross-build hospes-cdm, hospes-cdm-check and the
# player, copy them with the test CDM, its vectors and the Clear Key clips to a running guest,
# and run hospes/test-cdm/vm/run.sh there (the CDM transcript against the reference host's,
# then each clip through the player as `nobody`, with and without a worker kill).
#
#   ARCH=aarch64 sh scripts/hospes-h3-vm.sh     # or ARCH=amd64; the VM must be up
#
# The guest needs dav1d, libvpx, opus, libvorbis and libogg (pkg), and the sysroot their
# /usr/local copy, for the player's cross-build. In the cloud the guest's pkg needs this
# environment's proxy CA: SSL_CA_CERT_FILE=<the container's CA bundle, copied in> pkg install ...
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${ARCH:-aarch64}"
case "$ARCH" in
    aarch64|arm64) T=aarch64-unknown-freebsd; A=aarch64; S="$HOME/atrium-vm/sysroot" ;;
    *) T=x86_64-unknown-freebsd; A=x86_64; S="$HOME/atrium-vm-amd64/sysroot" ;;
esac
TU=$(echo "$T" | tr - _)
X="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
sh "$ROOT/scripts/hospes-build.sh" cdm "$T" >/dev/null
(cd "$ROOT/hospes" && RUSTFLAGS="$X" cargo build --release -p hospes-cdm-check --target "$T" >&2)
(cd "$ROOT/atrium-player" && env PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR="$S" PKG_CONFIG_LIBDIR="$S/usr/local/libdata/pkgconfig" \
    "CC_$TU=clang --target=$T --sysroot=$S" "CFLAGS_$TU=-I$S/usr/local/include" RUSTFLAGS="$X -C link-arg=-L$S/usr/local/lib" \
    cargo build --release --target "$T" -p atrium-player -p atrium-player-decode >&2)
B=$(mktemp -d)/h3vm
mkdir -p "$B/bin" "$B/vectors" "$B/media"
for f in hospes/target/$T/release/hospes-cdm hospes/target/$T/release/hospes-cdm-check \
         atrium-player/target/$T/release/atrium-player atrium-player/target/$T/release/atrium-player-decode; do
    cp "$ROOT/$f" "$B/bin/"
done
for f in "$B"/bin/*; do llvm-strip "$f" 2>/dev/null || true; done
cp "$ROOT/hospes/test-cdm/bin/$A-clearkey.so" "$B/"
cp "$ROOT"/hospes/test-cdm/vectors/* "$B/vectors/"
cp "$ROOT"/test-assets/media/clearkey/* "$B/media/"
for c in vp9-opus-frag.mp4 av1-opus.mp4 vp9-opus.webm; do cp "$ROOT/test-assets/media/$c.layer.framemd5" "$B/media/"; done
cp "$ROOT/hospes/test-cdm/vm/run.sh" "$B/"
tar czf - -C "$(dirname "$B")" h3vm | "$ROOT/scripts/vssh" 'rm -rf /tmp/h3vm && tar xzf - -C /tmp'
rm -rf "$(dirname "$B")"
"$ROOT/scripts/vssh" "cd /tmp/h3vm && ARCH=$A sh run.sh"
