#!/bin/sh
# Walk through the Insula platform end-to-end. Doubles
# as a smoke test (if anything in the user-facing CLI
# breaks, the demo dies loudly) and as documentation-
# by-example (a new user runs this once and watches
# the whole lifecycle scroll by).
#
# Usage:
#   scripts/insula-demo.sh           # quiet mode
#   scripts/insula-demo.sh --verbose # echo every step's command

set -eu

VERBOSE=0
for arg in "$@"; do
    case "$arg" in
        --verbose|-v) VERBOSE=1 ;;
        -h|--help)
            sed -n '2,12p' "$0"
            exit 0
            ;;
        *)
            printf 'unknown flag: %s\n' "$arg" >&2
            exit 2
            ;;
    esac
done

step() {
    printf '\n>>> %s\n' "$*"
}

cmd() {
    if [ "$VERBOSE" = 1 ]; then
        printf '$ %s\n' "$*"
    fi
    "$@"
}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

# Build the CLI + the daemons we'll need. Use the
# debug profile so the demo is fast even on a clean
# tree.
step "Building the CLI + platform daemons (debug profile)"
cargo build --quiet \
    --manifest-path insula-cli/Cargo.toml \
    --bin insula
for d in insula-logd vestibulum-macos atrium-netd-macos \
         praeco-macos tabellarius-macos; do
    cargo build --quiet \
        --manifest-path "$d/Cargo.toml" --bin "$d"
done

INSULA="$ROOT/insula-cli/target/debug/insula"

# Scratch install root + key dir so the demo never
# touches the user's real state.
INSTALL_ROOT=$(mktemp -d -t insula-demo-root.XXXXXX)
KEY_DIR=$(mktemp -d -t insula-demo-keys.XXXXXX)
APP_DIR=$(mktemp -d -t insula-demo-app.XXXXXX)
ARCHIVE="$APP_DIR.insula"

cleanup() {
    rc=$?
    if [ "$rc" != 0 ]; then
        printf '\n!!! demo failed (rc=%d), cleaning up\n' "$rc" >&2
    fi
    INSULA_INSTALL_ROOT="$INSTALL_ROOT" \
    INSULA_LOGD_BIN="$ROOT/insula-logd/target/debug/insula-logd" \
    INSULA_VESTIBULUMD_BIN="$ROOT/vestibulum-macos/target/debug/vestibulum-macos" \
    INSULA_NETD_BIN="$ROOT/atrium-netd-macos/target/debug/atrium-netd-macos" \
    INSULA_PRAECOD_BIN="$ROOT/praeco-macos/target/debug/praeco-macos" \
    INSULA_TABELLARIUSD_BIN="$ROOT/tabellarius-macos/target/debug/tabellarius-macos" \
    "$INSULA" clean --all >/dev/null 2>&1 || true
    rm -rf "$INSTALL_ROOT" "$KEY_DIR" "$APP_DIR" "$ARCHIVE"
}
trap cleanup EXIT INT TERM

export INSULA_INSTALL_ROOT="$INSTALL_ROOT"
export INSULA_LOGD_BIN="$ROOT/insula-logd/target/debug/insula-logd"
export INSULA_VESTIBULUMD_BIN="$ROOT/vestibulum-macos/target/debug/vestibulum-macos"
export INSULA_NETD_BIN="$ROOT/atrium-netd-macos/target/debug/atrium-netd-macos"
export INSULA_PRAECOD_BIN="$ROOT/praeco-macos/target/debug/praeco-macos"
export INSULA_TABELLARIUSD_BIN="$ROOT/tabellarius-macos/target/debug/tabellarius-macos"

step "Show CLI version + supported ABI surfaces"
cmd "$INSULA" version --verbose

step "Initialize a scratch Insula app at $APP_DIR"
rm -rf "$APP_DIR"
cmd "$INSULA" init "$APP_DIR" --name "com.example.demo-app"

step "Drop a real (shebang) binary at the declared entry path"
APP_BASENAME=$(basename "$APP_DIR")
printf '#!/bin/sh\necho hello from insula-demo\n' \
    > "$APP_DIR/bin/$APP_BASENAME"
chmod +x "$APP_DIR/bin/$APP_BASENAME"

step "Generate a publisher keypair + trust it"
cmd "$INSULA" keygen demo-publisher "$KEY_DIR"
cmd "$INSULA" publishers add demo-publisher "$KEY_DIR/demo-publisher.pub"
cmd "$INSULA" publishers list

step "Release: sign + pack into a single .insula archive"
cmd "$INSULA" release "$APP_DIR" "$ARCHIVE" \
    --key "$KEY_DIR/demo-publisher.sk"

step "Inspect the archive (without installing)"
cmd "$INSULA" info "$ARCHIVE"

step "Install the archive (signature verified end-to-end)"
cmd "$INSULA" install "$ARCHIVE"

step "List installed apps with capability tags"
cmd "$INSULA" list

step "Run the health check"
cmd "$INSULA" doctor

step "Uninstall the demo app"
cmd "$INSULA" uninstall com.example.demo-app

step "Confirm the install root is empty again"
cmd "$INSULA" list

printf '\n=== demo completed successfully ===\n'
