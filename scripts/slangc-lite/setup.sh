#!/bin/sh
# Build a minimal `slangc` for machines without the Slang release binaries
# (e.g. the Linux cloud containers, where GitHub release downloads are
# blocked). It links the official Slang compiler library shipped in the
# shader-slang `slangpy` wheel on PyPI and forwards slangc's arguments to it.
#
#   scripts/slangc-lite/setup.sh            # builds into $SLANGC_LITE_DIR
#   SLANGC=$SLANGC_LITE_DIR/slangc bundles/atrium-core/build.sh
#
# Supports what bundles/*/build.sh uses: <src> -target spirv -entry E
# -stage S -o OUT. Not a full slangc replacement.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
dir="${SLANGC_LITE_DIR:-$HOME/.cache/atrium-slangc-lite}"
ver="${SLANGPY_VERSION:-0.43.1}"
mkdir -p "$dir/wheel"
if [ ! -e "$dir/wheel/slangpy/include/slang.h" ]; then
    python3 -m pip download --no-deps --only-binary=:all: \
        --python-version 3.13 --platform manylinux_2_34_x86_64 \
        -d "$dir/dl" "slangpy==$ver"
    python3 -I -m zipfile -e "$dir"/dl/slangpy-"$ver"-*.whl "$dir/wheel"
fi
lib="$(ls "$dir"/wheel/slangpy/libslang-compiler.so.* | head -1)"
c++ -O1 -I"$dir/wheel/slangpy/include" "$here/slangc_lite.cpp" -o "$dir/slangc" \
    "$lib" -Wl,-rpath,"$dir/wheel/slangpy"
echo "built $dir/slangc"
