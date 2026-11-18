#!/bin/sh
# Build the CJK font packages for the Atrium catalog (docs/spec/fonts.md §5,
# docs/spec/opifex.md §8.1): Noto Sans JP, SC, TC, HK and KR, Regular and Bold,
# static TrueType (OFL 1.1), from the @expo-google-fonts npm packages, which
# carry Google Fonts' static instances. Each tarball is pinned by version and
# its npm sha512; nothing else is taken from it but the two .ttf files and the
# font licence.
#
#   sh scripts/opifex-cjk-fonts.sh <out-dir> [--sign <platform key>]
#
# <out-dir>/<id>/ is the packed package and <out-dir>/<id>.insula its archive;
# <out-dir>/incoming/<id>.toml is the catalog release request once the archive
# is uploaded to atrium-os/catalog-blobs (fill in nothing: the URL and sha256
# are written here). --sign takes the platform key's PEM or token key file
# (opifex token ref); without it the packages are left unsigned.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:?usage: opifex-cjk-fonts.sh <out-dir> [--sign <key>]}
shift
SIGN=""
[ "${1:-}" = --sign ] && SIGN=${2:?--sign needs a key file}
VER=0.4.4
PKGVER=2.004        # the fonts' own version (name table of the 0.4.4 TTFs)
BLOBS=https://github.com/atrium-os/catalog-blobs/releases/download/fonts-noto-cjk-$PKGVER
OPIFEX=${OPIFEX:-}
if [ -z "$OPIFEX" ]; then
    (cd "$ROOT/portcullis" && cargo build -q --bin opifex)
    OPIFEX=${CARGO_TARGET_DIR:-$ROOT/portcullis/target}/debug/opifex
fi
mkdir -p "$OUT/dl" "$OUT/incoming"

# Code points every CJK face is for: punctuation and symbols, Han, full-width forms.
HAN="2E80-2FDF,3000-303F,3190-319F,31C0-31EF,3200-33FF,3400-4DBF,4E00-9FFF,F900-FAFF,FE30-FE4F,FF00-FFEF,20000-2FA1F"

sha512_b64() { openssl dgst -sha512 -binary "$1" | openssl base64 -A; }

for spec in \
  "jp|Japanese|ja|3040-309F,30A0-30FF,31F0-31FF|1vl2U7uYG5JAXGERZaLMXtfOSgyOU4Qhq30OOPrbAHjOu8AW9Pral/qN9Anta9fjd6eVRTijiNqcgy2TowzrDA==" \
  "sc|Simplified Chinese|zh-Hans|3100-312F|LF4+p6xdtgUArufW0K+l9/FeDiXrW8FcEkEbOPIUC5ZnPupeFVA6yY9qCZ0sBrdAeLdlusDhv0R7q2x3onXo3w==" \
  "tc|Traditional Chinese|zh-Hant|3100-312F,31A0-31BF|mTYARqbwmV05CyyMFEpPtRz2QdtBZCIL3AgT1nzE2QyB2uhA7KdBBzr49ON41aIP+1xnJ6z8AtWYICk4DjygAQ==" \
  "hk|Hong Kong Chinese|zh-HK|3100-312F,31A0-31BF|lCAnYw/j3lC5ebdKHEY6FxMBC8AeFG2u6ugcDAJyj1+fX/5yIOWZmOPJlHNjKr+H8jwzCe/8URKt8XXlTmRz9w==" \
  "kr|Korean|ko|1100-11FF,3130-318F,A960-A97F,AC00-D7AF,D7B0-D7FF|zePBzeh74ja/0Jiw3T/9+V6Ie3+eONlIl5o1dQQYsOCmfXjJttMz0rGv1oB2AWlzX/TPiL7J3IP3M9Ga7CBvWA=="
do
    IFS='|' read -r l lang tag own sum <<EOF
$spec
EOF
    U=$(echo "$l" | tr a-z A-Z)
    id=org.atrium.font.noto-sans-$l
    tgz=$OUT/dl/noto-sans-$l-$VER.tgz
    [ -f "$tgz" ] || curl -fsS -o "$tgz" "https://registry.npmjs.org/@expo-google-fonts/noto-sans-$l/-/noto-sans-$l-$VER.tgz"
    [ "$(sha512_b64 "$tgz")" = "$sum" ] || { echo "$tgz: sha512 is not the pinned one"; exit 1; }
    rm -rf "$OUT/$id" "$OUT/x"; mkdir -p "$OUT/$id" "$OUT/x"
    tar xzf "$tgz" -C "$OUT/x" package/400Regular/NotoSans${U}_400Regular.ttf package/700Bold/NotoSans${U}_700Bold.ttf package/LICENSE_FONT
    cp "$OUT/x/package/400Regular/NotoSans${U}_400Regular.ttf" "$OUT/$id/NotoSans$U-Regular.ttf"
    cp "$OUT/x/package/700Bold/NotoSans${U}_700Bold.ttf" "$OUT/$id/NotoSans$U-Bold.ttf"
    cp "$OUT/x/package/LICENSE_FONT" "$OUT/$id/OFL.txt"
    rm -rf "$OUT/x"
    cat > "$OUT/$id.toml" <<EOF
kind = "font"
[font]
id = "$id"
name = "Noto Sans $U"
version = "$PKGVER"
license = "OFL-1.1"
description = "$lang text in Noto Sans, regular and bold."
languages = ["$tag"]

[[face]]
family = "Noto Sans $U"
file = "NotoSans$U-Regular.ttf"
weight = 400
ranges = "$own,$HAN"

[[face]]
family = "Noto Sans $U"
file = "NotoSans$U-Bold.ttf"
weight = 700
ranges = "$own,$HAN"
EOF
    if [ -n "$SIGN" ]; then
        "$OPIFEX" font pack "$OUT/$id" --manifest "$OUT/$id.toml" --sign "$SIGN" >/dev/null
    else
        "$OPIFEX" font pack "$OUT/$id" --manifest "$OUT/$id.toml" >/dev/null
    fi
    "$OPIFEX" archive "$OUT/$id" "$OUT/$id.insula" >/dev/null
    sha=$(openssl dgst -sha256 -r "$OUT/$id.insula" | cut -d' ' -f1)
    printf 'archive = "%s/%s.insula"\nsha256  = "%s"\narch    = "any"\n' "$BLOBS" "$id" "$sha" > "$OUT/incoming/$id.toml"
    echo "$id $PKGVER: $(du -k "$OUT/$id.insula" | cut -f1) KiB, sha256 $sha"
done
[ -n "$SIGN" ] || echo "(unsigned: pass --sign <platform key> to sign them)"
