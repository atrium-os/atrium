#!/bin/sh
# Pack the Rust layer trees that build-rust-layer.sh left in $WORK/layer
# (dev-tools-plan.md M3d): manifest, content index, signature (SIGN_KEY),
# LICENSES.md, then OUT/rust-<version>-<arch>.tar.xz with a .sha256 beside it.
# The SDK's own tools, cargo-atrium and atrium-schema, go into each tree's
# bin/ first, cross-built for its arch with `cargo atrium cross` (aarch64
# with -Zbuild-std, so the tree's rust-toolchain.toml nightly and the SDK
# sysroots in ~/.atrium/sysroot/<arch> are needed). NO_SDK_TOOLS=1 skips it.
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
if [ -z "${NO_SDK_TOOLS:-}" ]; then
    (cd "$ROOT/sdk" && cargo build -q -p cargo-atrium)
    CA="$ROOT/sdk/target/debug/cargo-atrium"
fi
for a in amd64 aarch64; do
    L="$WORK/layer/rust-$VERSION-$a"
    [ -f "$L/bin/rustc" ] || { echo "pack-rust-layer: no $L/bin/rustc (run build-rust-layer.sh)" >&2; exit 1; }
    if [ -z "${NO_SDK_TOOLS:-}" ]; then
        case $a in amd64) t=x86_64-unknown-freebsd; z= ;; aarch64) t=aarch64-unknown-freebsd; z=-Zbuild-std ;; esac
        # RUSTFLAGS="": the tree's .cargo/config.toml has link flags of its own.
        (cd "$ROOT/sdk" && RUSTFLAGS= "$CA" atrium cross "$a" -q --release $z -p cargo-atrium -p atrium-schema >/dev/null)
        for b in cargo-atrium atrium-schema; do
            install -m 0755 "$ROOT/sdk/target/$t/release/$b" "$L/bin/$b"
        done
    fi
    NAME="rust-$VERSION-$a"
    cat > "$L/LICENSES.md" <<T
# Rust $VERSION ($a): what is in this layer

rustc, cargo, clippy, rustfmt, rust-analyzer, std for x86_64- and
aarch64-unknown-freebsd, and the std source (for rust-analyzer). Built from
rustc-$VERSION-src.tar.xz, checked against the Rust release key
(108F66205EAEB0AAA8DD5E1C85AB96E6FA1BE5FE), with LLVM from that tarball.
MIT OR Apache-2.0: see LICENSE-MIT, LICENSE-APACHE and COPYRIGHT (which
lists the bundled LLVM and other third-party licences).

bin/cargo-atrium and bin/atrium-schema are the Atrium SDK's own tools
(MIT OR Apache-2.0), built from the Atrium tree's sdk/.
T
    cat > "$OUT/$NAME.toml" <<T
kind = "layer"

[layer]
name = "rust"
version = "$VERSION"
arch = "$a"
mount = "/atrium/toolchains/rust"
path = ["bin"]
summary = "Rust $VERSION: rustc, cargo, clippy, rustfmt, rust-analyzer, cargo-atrium; std for amd64 and aarch64"

[layer.env]
RUST_SRC_PATH = "\${LAYER}/lib/rustlib/src/rust/library"
T
    "$OPIFEX" layer pack "$L" --manifest "$OUT/$NAME.toml" ${SIGN_KEY:+--sign "$SIGN_KEY"}
    (cd "$WORK/layer" && tar -cf - "$NAME" | xz -T0 -6 > "$OUT/$NAME.tar.xz")
    (cd "$OUT" && sha256sum "$NAME.tar.xz" > "$NAME.tar.xz.sha256")
    echo "rust: $OUT/$NAME.tar.xz ($(du -h "$OUT/$NAME.tar.xz" | cut -f1))"
done
