#!/bin/sh
# The SDK crate registry (dev-tools.md §5.4, opifex.md §8.4): every crate an
# app can depend on, and the path crates they reach, packed into a static
# sparse registry under OUT, index signed with SIGN_KEY (the platform key).
#
#   sh scripts/sdk/build-registry.sh OUT URL        env SIGN_KEY
#
# URL is where OUT is served, e.g. https://atrium-os.github.io/catalog/sdk.
# Check the result with: cargo atrium registry verify OUT --keys <dir>.
set -eu
OUT=${1:?out dir}
URL=${2:?url OUT is served at}
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
# The crates the SDK offers to apps (dev-tools.md §5.4); everything they
# reach through path dependencies comes along.
CRATES="pergola libatrium atrium-fonts fresco-client fresco-protocol atrium-message atrium-worker atrium-open"
SEED=$(mktemp -d)
trap 'rm -rf "$SEED"' EXIT
mkdir -p "$SEED/src"
: > "$SEED/src/lib.rs"
{
    echo '[package]'
    echo 'name = "atrium-sdk-seed"'
    echo 'version = "0.0.0"'
    echo 'edition = "2021"'
    echo 'publish = false'
    echo '[dependencies]'
    for c in $CRATES; do
        case $c in atrium-message*|atrium-worker) p="$ROOT/sdk/$c" ;; *) p="$ROOT/$c" ;; esac
        echo "$c = { path = \"$p\" }"
    done
} > "$SEED/Cargo.toml"
(cd "$ROOT/sdk" && cargo build -q -p cargo-atrium)
"$ROOT/sdk/target/debug/cargo-atrium" atrium registry build --out "$OUT" --url "$URL" ${SIGN_KEY:+--sign "$SIGN_KEY"} "$SEED"
