#!/bin/sh
# Hospes version watch (docs/spec/hospes.md §11.4, H5): take today's libraries from their
# publishers and check that Hospes still hosts them. Run by
# .github/workflows/hospes-version-watch.yml every day; runs the same on a Linux machine.
#
#   sh scripts/hospes-version-watch.sh ARCH        # ARCH: x86_64 or aarch64
#
# For ARCH it:
#   1. builds hospes-install for ARCH (aarch64 on an x86_64 machine runs under qemu-user);
#   2. Widevine: fetches Google's signed apt index and the google-chrome-stable .deb named in
#      it, checks the chain (pinned key, Packages hash, .deb hash), takes out the CDM and
#      checks its ELF, its CDM entry points and every strong import against the shim
#      (`hospes-install widevine-check`). Nothing from the package is run;
#   3. OpenH264: fetches Cisco's binary and checks its imports the same way
#      (`hospes-install check-imports`), then decodes test-assets/media/h264-baseline.mp4
#      with it twice, under Hospes (hospes-codec --file) and under real glibc
#      (hospes/probe/h264host.c), and requires the two outputs to be identical.
#
# Any import the shim does not provide, a broken chain or a different decode fails the run.
# The fixture workload (scripts/hospes-build.sh test-linux) is a separate CI step.
#
# The Widevine CDM is never run here: running it outside the cloud session approved for H4
# needs Shankar's say-so. The libraries are fetched into a temporary directory and never
# committed. OPENH264_VERSION (default 2.6.0) picks Cisco's release; OPENH264_BZ2_DIR takes
# Cisco's .bz2 files from a directory instead (a machine that cannot reach Cisco's server).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
H="$ROOT/hospes"
ARCH="${1:?usage: hospes-version-watch.sh x86_64|aarch64}"
OH="${OPENH264_VERSION:-2.6.0}"
APT=https://dl.google.com/linux/chrome/deb
CISCO=http://ciscobinary.openh264.org
case "$ARCH" in
    x86_64) DEBARCH=amd64; OHNAME="libopenh264-$OH-linux64.8.so"; T=x86_64-unknown-linux-gnu; CC=cc ;;
    aarch64) DEBARCH=arm64; OHNAME="libopenh264-$OH-linux-arm64.8.so"; T=aarch64-unknown-linux-gnu; CC=aarch64-linux-gnu-gcc ;;
    *) echo "ARCH must be x86_64 or aarch64" >&2; exit 2 ;;
esac
RUN=""
if [ "$(uname -m)" != "$ARCH" ] && ! { [ "$ARCH" = aarch64 ] && [ "$(uname -m)" = arm64 ]; }; then
    RUN="qemu-$ARCH-static -L /usr/$ARCH-linux-gnu"
fi
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
say() { echo "== $*" >&2; }
summary() { echo "$*"; [ -n "${GITHUB_STEP_SUMMARY:-}" ] && echo "- $ARCH: $*" >> "$GITHUB_STEP_SUMMARY"; return 0; }

say "hospes-install for $T"
(cd "$H" && CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    cargo build -q --release -p hospes-install --target "$T")
HI="$H/target/$T/release/hospes-install"

say "Widevine: Google's apt index ($DEBARCH)"
curl -sSf --retry 3 -o "$W/InRelease" "$APT/dists/stable/InRelease"
curl -sSf --retry 3 -o "$W/Packages" "$APT/dists/stable/main/binary-$DEBARCH/Packages"
set -- $($RUN "$HI" widevine-index "$W/InRelease" "$W/Packages")
say "Widevine: $1 ($2 bytes)"
curl -sSf --retry 3 -o "$W/chrome.deb" "$APT/$1"
summary "$($RUN "$HI" widevine-check "$W/InRelease" "$W/Packages" "$W/chrome.deb")"
rm -f "$W/chrome.deb"

say "OpenH264 $OH: $OHNAME"
if [ -n "${OPENH264_BZ2_DIR:-}" ]; then cp "$OPENH264_BZ2_DIR/$OHNAME.bz2" "$W/oh.so.bz2"
else curl -sSf --retry 3 -o "$W/oh.so.bz2" "$CISCO/$OHNAME.bz2"; fi
bunzip2 "$W/oh.so.bz2"
summary "OpenH264 $OH: $($RUN "$HI" check-imports "$W/oh.so" | sed 's/^[^:]*: //')"

say "OpenH264 $OH: decode under Hospes and under glibc"
CODEC=$(sh "$ROOT/scripts/hospes-build.sh" codec "$T")
$CC -O1 -I"$H/probe/third_party" -o "$W/h264host" "$H/probe/h264host.c" -ldl
ffmpeg -v error -i "$ROOT/test-assets/media/h264-baseline.mp4" -map 0:v -c:v copy -bsf:v h264_mp4toannexb -f h264 "$W/in.264"
$RUN "$W/h264host" "$W/oh.so" "$W/in.264" 1 "$W/ref.yuv" > /dev/null
$RUN "$CODEC" --file "$W/oh.so" "$W/in.264" "$W/got.yuv" 2> "$W/codec.log" || { cat "$W/codec.log" >&2; exit 1; }
ref=$(sha256sum < "$W/ref.yuv" | cut -d' ' -f1)
got=$(sha256sum < "$W/got.yuv" | cut -d' ' -f1)
if [ -s "$W/ref.yuv" ] && [ "$ref" = "$got" ]; then
    summary "OpenH264 $OH: h264-baseline.mp4 decodes identically under Hospes and glibc ($(wc -c < "$W/got.yuv") bytes of I420)"
else
    summary "OpenH264 $OH: h264-baseline.mp4 DIFFERS under Hospes (glibc $ref, Hospes $got)"
    exit 1
fi
