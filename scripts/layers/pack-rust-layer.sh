#!/bin/sh
# Pack the Rust layer trees that build-rust-layer.sh left in $WORK/layer
# (dev-tools-plan.md M3d): manifest, content index, signature (SIGN_KEY),
# LICENSES.md, then OUT/rust-<version>-<arch>.tar.xz with a .sha256 beside it.
#
#   sh pack-rust-layer.sh [VERSION] [OUT]      env WORK, SIGN_KEY, OPIFEX
set -eu
VERSION=${1:-1.100.0}
WORK=${WORK:-$HOME/work/rust-layer}
OUT=${2:-$WORK/out}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
mkdir -p "$OUT"
OPIFEX=${OPIFEX:-}
if [ -z "$OPIFEX" ]; then
    (cd "$ROOT/portcullis" && cargo build -q -p opifex)
    OPIFEX="$ROOT/portcullis/target/debug/opifex"
fi
for a in amd64 aarch64; do
    L="$WORK/layer/rust-$VERSION-$a"
    [ -f "$L/bin/rustc" ] || { echo "pack-rust-layer: no $L/bin/rustc (run build-rust-layer.sh)" >&2; exit 1; }
    NAME="rust-$VERSION-$a"
    cat > "$L/LICENSES.md" <<T
# Rust $VERSION ($a): what is in this layer

rustc, cargo, clippy, rustfmt, rust-analyzer, std for x86_64- and
aarch64-unknown-freebsd, and the std source (for rust-analyzer). Built from
rustc-$VERSION-src.tar.xz, checked against the Rust release key
(108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE), with LLVM from that tarball.
MIT OR Apache-2.0: see LICENSE-MIT, LICENSE-APACHE and COPYRIGHT (which
lists the bundled LLVM and other third-party licences).
T
    cat > "$OUT/$NAME.toml" <<T
kind = "layer"

[layer]
name = "rust"
version = "$VERSION"
arch = "$a"
mount = "/atrium/toolchains/rust"
path = ["bin"]
summary = "Rust $VERSION: rustc, cargo, clippy, rustfmt, rust-analyzer; std for amd64 and aarch64"

[layer.env]
RUST_SRC_PATH = "\${LAYER}/lib/rustlib/src/rust/library"
T
    "$OPIFEX" layer pack "$L" --manifest "$OUT/$NAME.toml" ${SIGN_KEY:+--sign "$SIGN_KEY"}
    (cd "$WORK/layer" && tar -cf - "$NAME" | xz -T0 -6 > "$OUT/$NAME.tar.xz")
    (cd "$OUT" && sha256sum "$NAME.tar.xz" > "$NAME.tar.xz.sha256")
    echo "rust: $OUT/$NAME.tar.xz ($(du -h "$OUT/$NAME.tar.xz" | cut -f1))"
done
