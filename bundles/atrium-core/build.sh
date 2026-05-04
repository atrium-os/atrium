#!/bin/sh
# Compile atrium-core's GLSL sources to SPIR-V via glslangValidator.
#
# .spv files land alongside their .comp/.vert/.frag sources and are
# .gitignored — rebuild from source on every developer machine.
#
# Run from anywhere:
#   ~/src/fresco-poc/bundles/atrium-core/build.sh
#
# Step 4's bundle loader reads the .spv files at server startup and
# validates them with spirv-val before AOT-compiling pipelines.

set -eu

cd "$(dirname "$0")"

GLSLANG="${GLSLANG:-glslangValidator}"
SPIRV_VAL="${SPIRV_VAL:-spirv-val}"

if ! command -v "$GLSLANG" >/dev/null; then
    echo "error: $GLSLANG not in PATH" >&2
    echo "       brew install glslang  (macOS) / pkg install glslang (FreeBSD)" >&2
    exit 1
fi

# Vulkan target (so SPIR-V uses Vulkan-style binding decorations,
# entry-point semantics, etc., not the OpenGL defaults).
VFLAGS="-V --target-env vulkan1.3"

compile() {
    src=$1
    # Append .spv (don't replace extension), so .vert/.frag/.comp
    # are preserved in the output name. Otherwise `pipe_rectangle.vert`
    # and `pipe_rectangle.frag` would BOTH compile to the same
    # `pipe_rectangle.spv` and clobber each other.
    out="${src}.spv"
    echo "  $src → ${out#./}"
    "$GLSLANG" $VFLAGS -o "$out" "$src" >/dev/null
    if command -v "$SPIRV_VAL" >/dev/null; then
        "$SPIRV_VAL" "$out"
    fi
}

echo "compiling compute shaders:"
for f in compute/*.comp; do
    compile "$f"
done

echo "compiling render pipelines:"
for f in pipelines/*.vert pipelines/*.frag; do
    [ -e "$f" ] || continue
    compile "$f"
done

echo "ok — all .spv built and validated"
