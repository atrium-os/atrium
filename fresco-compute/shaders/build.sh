#!/bin/sh
# Rebuild the committed SPIR-V. Needs glslangValidator (glslang-tools) and
# spirv-opt (spirv-tools). -O inlines the helper functions and promotes locals
# to SSA, which keeps the Tier-2 and GIR frontends on their well-trodden path.
set -e
cd "$(dirname "$0")"
for s in bin raster; do
    glslangValidator -V --target-env vulkan1.1 -o "$s.comp.raw.spv" "$s.comp"
    spirv-opt -O "$s.comp.raw.spv" -o "$s.comp.spv"
    spirv-val "$s.comp.spv"
    rm "$s.comp.raw.spv"
done
