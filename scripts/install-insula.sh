#!/bin/sh
# Install the Insula CLI + platform daemons + libatrium
# from a freshly-built ./dist/ tree to a user prefix.
# Mirror image: --uninstall removes them.
#
# Usage:
#   scripts/install-insula.sh                  # to ~/.local
#   scripts/install-insula.sh --prefix /opt    # custom prefix
#   scripts/install-insula.sh --uninstall      # remove
#   scripts/install-insula.sh --uninstall --prefix /opt
#
# Default prefix is ~/.local because writing to
# /usr/local/bin needs sudo on modern macOS and we
# want this script to be safe to run without it. Root
# users who want system-wide install can pass
# --prefix /usr/local.
#
# Run scripts/build-insula.sh first; this script
# assumes ./dist/ is populated.

set -eu

PREFIX="$HOME/.local"
MODE=install
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix=*)
            PREFIX=${1#*=}
            ;;
        --prefix)
            shift
            if [ $# -eq 0 ]; then
                printf -- '--prefix needs a value\n' >&2
                exit 2
            fi
            PREFIX="$1"
            ;;
        --uninstall|-u)
            MODE=uninstall
            ;;
        -h|--help)
            sed -n '2,18p' "$0"
            exit 0
            ;;
        *)
            printf 'unknown arg: %s\n' "$1" >&2
            exit 2
            ;;
    esac
    shift
done

BIN_DIR="$PREFIX/bin"
LIB_DIR="$PREFIX/lib"
INC_DIR="$PREFIX/include"

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 1

if [ ! -d "dist" ]; then
    printf 'no ./dist/ tree — run scripts/build-insula.sh first\n' >&2
    exit 1
fi

BINS="insula insula-logd vestibulum-macos atrium-netd-macos \
      praeco-macos tabellarius-macos insula-hello atrium-fetch \
      atrium-mon atrium-paint insula-clock"
LIBS="libatrium.dylib libatrium.so libatrium.a"
HEADERS="atrium.h"

case "$MODE" in
    install)
        mkdir -p "$BIN_DIR" "$LIB_DIR" "$INC_DIR"
        echo "installing into $PREFIX:"

        for b in $BINS; do
            src="dist/bin/$b"
            if [ -f "$src" ]; then
                cp "$src" "$BIN_DIR/"
                chmod +x "$BIN_DIR/$b"
                printf '  bin/%s\n' "$b"
            fi
        done

        for l in $LIBS; do
            src="dist/lib/$l"
            if [ -f "$src" ]; then
                cp "$src" "$LIB_DIR/"
                printf '  lib/%s\n' "$l"
            fi
        done

        for h in $HEADERS; do
            src="dist/include/$h"
            if [ -f "$src" ]; then
                cp "$src" "$INC_DIR/"
                printf '  include/%s\n' "$h"
            fi
        done

        echo
        echo "done. Add these to your shell profile if not already:"
        echo "  export PATH=\"$BIN_DIR:\$PATH\""
        case "$PREFIX" in
            /usr/local|/usr) ;;
            *) echo "  export LIBRARY_PATH=\"$LIB_DIR:\$LIBRARY_PATH\""
               echo "  export DYLD_FALLBACK_LIBRARY_PATH=\"$LIB_DIR:\$DYLD_FALLBACK_LIBRARY_PATH\""
               ;;
        esac
        ;;

    uninstall)
        echo "removing from $PREFIX:"
        for b in $BINS; do
            target="$BIN_DIR/$b"
            if [ -f "$target" ]; then
                rm -f "$target"
                printf '  bin/%s\n' "$b"
            fi
        done
        for l in $LIBS; do
            target="$LIB_DIR/$l"
            if [ -f "$target" ]; then
                rm -f "$target"
                printf '  lib/%s\n' "$l"
            fi
        done
        for h in $HEADERS; do
            target="$INC_DIR/$h"
            if [ -f "$target" ]; then
                rm -f "$target"
                printf '  include/%s\n' "$h"
            fi
        done
        echo
        echo "done. Bin/lib/include directories themselves were left in"
        echo "place (other tools may rely on them)."
        ;;
esac
