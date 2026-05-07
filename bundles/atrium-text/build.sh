#!/bin/sh
# Compile atrium-core's Slang sources to SPIR-V via slangc.
#
# Why Slang (decided 2026-05-07):
#  - Vulkan's actual native input is SPIR-V; the source language is
#    our choice. None of GLSL/HLSL/Slang/WGSL is privileged.
#  - Slang is shader-specific (no GL/DX heritage), Khronos-stewarded,
#    Apache-2.0 with LLVM exception (matches Atrium's permissive-only
#    licensing policy).
#  - Multi-backend emit (SPIR-V / DXIL / Metal / CUDA from one source)
#    means future Atrium-on-Metal / -on-Direct3D paths reuse these
#    shaders unchanged.
#
# Migration history:
#  - pre-2026-05-07: GLSL via glslangValidator.
#  - 2026-05-07:     migrated to Slang. Same SPIR-V output binary
#                    interface; renderer code unchanged.
#
# CRITICAL: do NOT pass `-profile glsl_460` to slangc. That flag forces
# the legacy `BufferBlock + Uniform SC` SSBO encoding, which fresco-
# vulkan's reflect.rs treats as a UBO, blowing the descriptor pool.
# Plain `-target spirv` emits the modern `Block + StorageBuffer SC`
# style that matches what glslangValidator was producing.
#
# .spv files land alongside their .slang sources and are .gitignored;
# rebuild from source on every developer machine.

set -eu

cd "$(dirname "$0")"

SLANGC="${SLANGC:-slangc}"
SPIRV_VAL="${SPIRV_VAL:-spirv-val}"

if ! command -v "$SLANGC" >/dev/null; then
    if [ -x "$HOME/src/slang-bin/bin/slangc" ]; then
        SLANGC="$HOME/src/slang-bin/bin/slangc"
    else
        echo "error: slangc not in PATH and not at \$HOME/src/slang-bin/bin/" >&2
        echo "       see https://github.com/shader-slang/slang/releases" >&2
        exit 1
    fi
fi

compile_pipe() {
    src="$1"                 # e.g. pipelines/pipe_rectangle.slang
    base="${src%.slang}"
    echo "  $src → ${base}.{vert,frag}.spv"
    "$SLANGC" "$src" -target spirv -entry vert_main -stage vertex   -o "${base}.vert.spv"
    "$SLANGC" "$src" -target spirv -entry frag_main -stage fragment -o "${base}.frag.spv"
    if command -v "$SPIRV_VAL" >/dev/null; then
        "$SPIRV_VAL" "${base}.vert.spv"
        "$SPIRV_VAL" "${base}.frag.spv"
    fi
}

compile_compute() {
    src="$1"
    base="${src%.slang}"
    echo "  $src → ${base}.comp.spv"
    "$SLANGC" "$src" -target spirv -entry comp_main -stage compute -o "${base}.comp.spv"
    if command -v "$SPIRV_VAL" >/dev/null; then
        "$SPIRV_VAL" "${base}.comp.spv"
    fi
}

echo "compiling render pipelines:"
for f in pipelines/*.slang; do
    [ -e "$f" ] || continue
    compile_pipe "$f"
done

echo "compiling compute kernels:"
for f in compute/*.slang; do
    [ -e "$f" ] || continue
    compile_compute "$f"
done

echo "ok — all .spv built and validated"
