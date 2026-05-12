#!/bin/sh
# atrium-pkg-precompile-hook.sh — reference implementation of the
# shader-precompile install hook described in
# `docs/spec/atrium-pkg.md` §3.6.5.
#
# Usage:
#   atrium-pkg-precompile-hook.sh <BUNDLE_DIR> [CACHE_DIR]
#
#   BUNDLE_DIR  Directory containing a manifest.json and either
#               pre-built .spv files OR .slang sources alongside a
#               build.sh that compiles them.
#   CACHE_DIR   Where to install validated shaders. Defaults to
#               $XDG_CACHE_HOME/atrium/shaders or
#               $HOME/.cache/atrium/shaders.
#
# Pipeline:
#
#   1. Compile      — if BUNDLE_DIR/build.sh exists, invoke it.
#                     build.sh is expected to call slangc + the
#                     annotate tool per bundles/atrium-{core,text}/
#                     build.sh's pattern.
#   2. Verify       — `aqueduct-shader-tool verify-bundle BUNDLE_DIR`
#                     parses manifest.json, validates the structural
#                     schema, and runs every referenced .spv through
#                     the strict-mode validator.
#   3. Precompile   — `aqueduct-shader-tool precompile --cache CACHE
#                     --backend NAME BUNDLE_DIR` populates the shader
#                     cache for the detected host backend(s).
#                     Subsequent OP_GPU_SHADER_RESOLVE calls hit
#                     warm.
#
# Atomicity: if Verify rejects any shader, the script aborts BEFORE
# touching the cache. This matches the spec's "install rollback":
# CAS-ingested content stays (harmless if unreferenced) but the
# manifest is not dropped and the registry doesn't record the install.
#
# Exit codes:
#   0  every shader compiled + validated + cached
#   1  validator rejected at least one shader (atomic abort)
#   2  I/O or usage error

set -eu

if [ $# -lt 1 ]; then
    echo "usage: $0 <BUNDLE_DIR> [CACHE_DIR]" >&2
    exit 2
fi

BUNDLE_DIR="$1"
if [ ! -d "$BUNDLE_DIR" ]; then
    echo "error: $BUNDLE_DIR is not a directory" >&2
    exit 2
fi
if [ ! -f "$BUNDLE_DIR/manifest.json" ]; then
    echo "error: $BUNDLE_DIR/manifest.json not found" >&2
    exit 2
fi

# Default cache dir, honouring XDG.
if [ $# -ge 2 ]; then
    CACHE_DIR="$2"
elif [ -n "${XDG_CACHE_HOME:-}" ]; then
    CACHE_DIR="$XDG_CACHE_HOME/atrium/shaders"
else
    CACHE_DIR="${HOME}/.cache/atrium/shaders"
fi

# Locate the shader tool. In production this is on $PATH; for
# in-tree dev work, fall back to the cargo-built binary.
SHADER_TOOL="${AQUEDUCT_SHADER_TOOL:-aqueduct-shader-tool}"
if ! command -v "$SHADER_TOOL" >/dev/null; then
    HERE="$(cd "$(dirname "$0")" && pwd)"
    if [ -x "$HERE/../target/debug/aqueduct-shader-tool" ]; then
        SHADER_TOOL="$HERE/../target/debug/aqueduct-shader-tool"
    elif [ -x "$HERE/../target/release/aqueduct-shader-tool" ]; then
        SHADER_TOOL="$HERE/../target/release/aqueduct-shader-tool"
    else
        echo "error: aqueduct-shader-tool not found; set AQUEDUCT_SHADER_TOOL" >&2
        exit 2
    fi
fi

# Detected host backend — for Phase 1.3b this is the SoftwareBackend
# fallback. Real impl probes IOC_GPU_LIST_BACKENDS via the kmod and
# runs precompile once per (vendor, generation) tuple.
BACKEND="${ATRIUM_BACKEND:-software}"
GENERATION="${ATRIUM_BACKEND_GENERATION:-0}"
COMPILER_VERSION="${ATRIUM_COMPILER_VERSION:-0}"

echo "atrium-pkg precompile hook"
echo "  bundle      : $BUNDLE_DIR"
echo "  cache       : $CACHE_DIR"
echo "  backend     : $BACKEND (gen=$GENERATION)"
echo "  compiler ver: $COMPILER_VERSION"
echo "  shader_tool : $SHADER_TOOL"
echo ""

# ── 1. Compile ────────────────────────────────────────────────────
if [ -x "$BUNDLE_DIR/build.sh" ]; then
    echo "── 1. Compiling .slang sources via build.sh ─────"
    AQUEDUCT_SHADER_TOOL="$SHADER_TOOL" "$BUNDLE_DIR/build.sh"
    echo ""
else
    echo "── 1. (no build.sh — assuming pre-built .spv files) ──"
    echo ""
fi

# ── 2. Verify ─────────────────────────────────────────────────────
echo "── 2. Verifying bundle manifest + every referenced shader ─────"
if ! "$SHADER_TOOL" verify-bundle "$BUNDLE_DIR"; then
    echo ""
    echo "atrium-pkg-precompile-hook: VERIFICATION FAILED — install aborted"
    echo "                            cache NOT populated"
    exit 1
fi
echo ""

# ── 3. Precompile / cache populate ────────────────────────────────
echo "── 3. Populating shader cache for backend=$BACKEND gen=$GENERATION ─────"
"$SHADER_TOOL" precompile \
    --cache            "$CACHE_DIR" \
    --backend          "$BACKEND" \
    --generation       "$GENERATION" \
    --compiler-version "$COMPILER_VERSION" \
    "$BUNDLE_DIR"

echo ""
echo "atrium-pkg-precompile-hook: SUCCESS"
echo "  bundle install can proceed; subsequent OP_GPU_SHADER_RESOLVE"
echo "  calls referencing these shaders will hit the cache warm."
